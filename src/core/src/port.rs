//! Port publishing validation.
//!
//! a3s-box currently supports Docker-style TCP port publishing in the
//! `host_port:guest_port[/tcp]` form. Unsupported protocols and bind-specific
//! host IPs are rejected before a box record is persisted or a VM boots.

/// Supported published-port protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProtocol {
    /// TCP port publishing.
    Tcp,
}

impl PortProtocol {
    /// String representation used by Docker-compatible output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
        }
    }
}

/// Parsed published-port mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    /// Host-side port. `0` means host auto-assignment where supported.
    pub host_port: u16,
    /// Guest/container port.
    pub guest_port: u16,
    /// Published protocol.
    pub protocol: PortProtocol,
}

impl PortMapping {
    /// Convert to the normalized runtime `host:guest` format.
    pub fn runtime_entry(&self) -> String {
        format!("{}:{}", self.host_port, self.guest_port)
    }
}

/// Validate and normalize multiple port mappings to runtime format.
pub fn normalize_port_maps(entries: &[String]) -> Result<Vec<String>, String> {
    entries
        .iter()
        .map(|entry| parse_port_mapping(entry).map(|mapping| mapping.runtime_entry()))
        .collect()
}

/// Parse a published-port mapping.
pub fn parse_port_mapping(input: &str) -> Result<PortMapping, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Invalid port mapping: value must not be empty".to_string());
    }

    let mut protocol_split = input.split('/');
    let port_part = protocol_split.next().unwrap_or_default();
    let protocol = match protocol_split.next() {
        None => PortProtocol::Tcp,
        Some(value) if value.eq_ignore_ascii_case("tcp") => PortProtocol::Tcp,
        Some("") => {
            return Err(format!(
                "Invalid port mapping '{input}': protocol must not be empty"
            ));
        }
        Some(value) => {
            return Err(format!(
                "Unsupported port mapping protocol '{value}' in '{input}'; only TCP is supported"
            ));
        }
    };
    if protocol_split.next().is_some() {
        return Err(format!(
            "Invalid port mapping '{input}': expected host_port:guest_port[/tcp]"
        ));
    }

    let parts: Vec<&str> = port_part.split(':').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid port mapping '{input}': expected host_port:guest_port[/tcp]; bind-specific host IPs, single-port shorthand, and port ranges are not supported"
        ));
    }

    let host_port = parse_port(input, parts[0], "host", true)?;
    let guest_port = parse_port(input, parts[1], "guest", false)?;

    Ok(PortMapping {
        host_port,
        guest_port,
        protocol,
    })
}

fn parse_port(input: &str, value: &str, label: &str, allow_zero: bool) -> Result<u16, String> {
    if value.is_empty() {
        return Err(format!(
            "Invalid port mapping '{input}': {label} port must not be empty"
        ));
    }
    if value.contains('-') {
        return Err(format!(
            "Invalid port mapping '{input}': {label} port ranges are not supported"
        ));
    }

    let port = value.parse::<u16>().map_err(|_| {
        format!("Invalid port mapping '{input}': {label} port '{value}' must be 0..=65535")
    })?;
    if port == 0 && !allow_zero {
        return Err(format!(
            "Invalid port mapping '{input}': guest port must be 1..=65535"
        ));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_port_mapping_host_guest() {
        let mapping = parse_port_mapping("8080:80").unwrap();

        assert_eq!(mapping.host_port, 8080);
        assert_eq!(mapping.guest_port, 80);
        assert_eq!(mapping.protocol, PortProtocol::Tcp);
        assert_eq!(mapping.protocol.as_str(), "tcp");
        assert_eq!(mapping.runtime_entry(), "8080:80");
    }

    #[test]
    fn test_parse_port_mapping_tcp_suffix_is_normalized() {
        let mapping = parse_port_mapping("8080:80/tcp").unwrap();

        assert_eq!(mapping.runtime_entry(), "8080:80");
    }

    #[test]
    fn test_parse_port_mapping_allows_host_port_zero() {
        let mapping = parse_port_mapping("0:8080").unwrap();

        assert_eq!(mapping.host_port, 0);
        assert_eq!(mapping.guest_port, 8080);
    }

    #[test]
    fn test_normalize_port_maps() {
        let entries = vec!["8080:80/tcp".to_string(), "8443:443".to_string()];

        let normalized = normalize_port_maps(&entries).unwrap();

        assert_eq!(normalized, vec!["8080:80", "8443:443"]);
    }

    #[test]
    fn test_parse_port_mapping_rejects_udp() {
        let error = parse_port_mapping("8080:80/udp").unwrap_err();

        assert!(error.contains("only TCP is supported"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_host_ip() {
        let error = parse_port_mapping("127.0.0.1:8080:80").unwrap_err();

        assert!(error.contains("bind-specific host IPs"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_single_port() {
        let error = parse_port_mapping("80").unwrap_err();

        assert!(error.contains("single-port shorthand"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_guest_zero() {
        let error = parse_port_mapping("8080:0").unwrap_err();

        assert!(error.contains("guest port"));
    }

    #[test]
    fn test_parse_port_mapping_rejects_ranges() {
        let error = parse_port_mapping("8000-8010:80").unwrap_err();

        assert!(error.contains("ranges"));
    }
}
