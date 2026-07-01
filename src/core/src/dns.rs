//! DNS configuration helpers for guest rootfs.
//!
//! Generates /etc/resolv.conf content from user-specified DNS servers,
//! host configuration, or sensible defaults.

use std::net::IpAddr;

/// Default DNS servers (Google Public DNS).
const DEFAULT_DNS: &[&str] = &["8.8.8.8", "8.8.4.4"];

/// A static host-to-IP mapping for `/etc/hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Hostname or DNS name.
    pub host: String,
    /// IP address string.
    pub ip: String,
}

/// Generate resolv.conf content for the guest rootfs.
///
/// Resolution order:
/// 1. If `custom_dns` is non-empty, use those servers
/// 2. Otherwise, try to read the host's /etc/resolv.conf
/// 3. Fall back to Google Public DNS (8.8.8.8, 8.8.4.4)
pub fn generate_resolv_conf(custom_dns: &[String]) -> String {
    if !custom_dns.is_empty() {
        return custom_dns
            .iter()
            .map(|s| format!("nameserver {s}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    }

    if let Some(host_resolv) = read_host_resolv_conf() {
        return host_resolv;
    }

    // Fallback to default DNS
    DEFAULT_DNS
        .iter()
        .map(|s| format!("nameserver {s}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Render `/etc/resolv.conf` content from explicit DNS settings.
///
/// Emits one `nameserver` line per server, a single `search` line (when any
/// search domains are given), and a single `options` line (when any options are
/// given) — the layout Kubernetes' `DNSConfig` expects. Returns an empty string
/// when nothing is configured, so callers can fall back to a default.
pub fn render_resolv_conf(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut out = String::new();
    for server in servers {
        out.push_str("nameserver ");
        out.push_str(server);
        out.push('\n');
    }
    if !searches.is_empty() {
        out.push_str("search ");
        out.push_str(&searches.join(" "));
        out.push('\n');
    }
    if !options.is_empty() {
        out.push_str("options ");
        out.push_str(&options.join(" "));
        out.push('\n');
    }
    out
}

/// Try to read the host's /etc/resolv.conf.
///
/// Returns None if the file doesn't exist, is unreadable, or contains
/// no nameserver entries (e.g., only comments).
fn read_host_resolv_conf() -> Option<String> {
    let content = std::fs::read_to_string("/etc/resolv.conf").ok()?;

    // Filter to only nameserver lines (skip comments, search, domain, etc.)
    let nameservers: Vec<&str> = content
        .lines()
        .filter(|line| line.trim_start().starts_with("nameserver"))
        .collect();

    if nameservers.is_empty() {
        return None;
    }

    Some(nameservers.join("\n") + "\n")
}

/// Generate /etc/hosts content for DNS service discovery.
///
/// Produces a hosts file with:
/// - localhost entry (127.0.0.1)
/// - the box's own IP and name
/// - peer entries for all other boxes on the same network
pub fn generate_hosts_file(
    own_ip: &str,
    own_name: &str,
    peers: &[(String, String)], // (ip, name)
) -> String {
    generate_hosts_file_with_entries(Some(own_ip), &[own_name.to_string()], peers, &[])
}

/// Generate `/etc/hosts` content with optional own aliases and static entries.
pub fn generate_hosts_file_with_entries(
    own_ip: Option<&str>,
    own_names: &[String],
    peers: &[(String, String)], // (ip, name)
    extra_hosts: &[HostEntry],
) -> String {
    let mut lines = Vec::new();
    lines.push("127.0.0.1 localhost".to_string());
    if !own_names.is_empty() {
        let own_names = own_names.join(" ");
        let own_ip = own_ip.unwrap_or("127.0.1.1");
        lines.push(format!("{} {}", own_ip, own_names));
    }
    for (ip, name) in peers {
        lines.push(format!("{} {}", ip, name));
    }
    for entry in extra_hosts {
        lines.push(format!("{} {}", entry.ip, entry.host));
    }
    lines.join("\n") + "\n"
}

/// Validate a hostname or DNS name accepted by a3s-box runtime options.
pub fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("hostname must not be empty".to_string());
    }
    if hostname.len() > 253 {
        return Err("hostname must be at most 253 characters".to_string());
    }
    if hostname.contains('\0') || hostname.chars().any(char::is_whitespace) {
        return Err("hostname must not contain whitespace or NUL bytes".to_string());
    }

    for label in hostname.trim_end_matches('.').split('.') {
        if label.is_empty() {
            return Err(format!("hostname '{hostname}' contains an empty label"));
        }
        if label.len() > 63 {
            return Err(format!(
                "hostname label '{label}' is longer than 63 characters"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "hostname label '{label}' must not start or end with '-'"
            ));
        }
        if !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!(
                "hostname label '{label}' contains unsupported characters"
            ));
        }
    }

    Ok(())
}

/// Parse a CLI `--add-host HOST:IP` value.
pub fn parse_add_host_entry(entry: &str) -> Result<HostEntry, String> {
    let (host, ip) = entry
        .split_once(':')
        .ok_or_else(|| format!("expected HOST:IP, got '{entry}'"))?;
    validate_hostname(host).map_err(|e| format!("invalid host '{host}': {e}"))?;
    let ip = ip.trim();
    if ip.is_empty() {
        return Err(format!("missing IP address in '{entry}'"));
    }
    ip.parse::<IpAddr>()
        .map_err(|_| format!("invalid IP address '{ip}' in '{entry}'"))?;

    Ok(HostEntry {
        host: host.to_string(),
        ip: ip.to_string(),
    })
}

/// Parse repeated CLI `--add-host` values.
pub fn parse_add_host_entries(entries: &[String]) -> Result<Vec<HostEntry>, String> {
    entries
        .iter()
        .map(|entry| parse_add_host_entry(entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_dns() {
        let result = generate_resolv_conf(&["1.1.1.1".to_string(), "1.0.0.1".to_string()]);
        assert_eq!(result, "nameserver 1.1.1.1\nnameserver 1.0.0.1\n");
    }

    #[test]
    fn test_render_resolv_conf() {
        let servers = vec!["10.10.10.10".to_string(), "10.10.10.11".to_string()];
        let searches = vec!["a.com".to_string(), "b.com".to_string()];
        let options = vec!["ndots:5".to_string(), "timeout:2".to_string()];
        assert_eq!(
            render_resolv_conf(&servers, &searches, &options),
            "nameserver 10.10.10.10\nnameserver 10.10.10.11\nsearch a.com b.com\noptions ndots:5 timeout:2\n"
        );
        // Servers only — no search/options lines.
        assert_eq!(
            render_resolv_conf(&["1.1.1.1".to_string()], &[], &[]),
            "nameserver 1.1.1.1\n"
        );
        // Nothing configured -> empty (caller falls back to a default).
        assert_eq!(render_resolv_conf(&[], &[], &[]), "");
    }

    #[test]
    fn test_empty_dns_uses_host_or_default() {
        let result = generate_resolv_conf(&[]);
        // Should contain at least one nameserver line
        assert!(result.contains("nameserver"));
    }

    #[test]
    fn test_single_dns() {
        let result = generate_resolv_conf(&["9.9.9.9".to_string()]);
        assert_eq!(result, "nameserver 9.9.9.9\n");
    }

    // --- generate_hosts_file tests ---

    #[test]
    fn test_hosts_file_no_peers() {
        let result = generate_hosts_file("10.88.0.2", "web", &[]);
        assert_eq!(result, "127.0.0.1 localhost\n10.88.0.2 web\n");
    }

    #[test]
    fn test_hosts_file_with_peers() {
        let peers = vec![
            ("10.88.0.3".to_string(), "api".to_string()),
            ("10.88.0.4".to_string(), "db".to_string()),
        ];
        let result = generate_hosts_file("10.88.0.2", "web", &peers);
        assert_eq!(
            result,
            "127.0.0.1 localhost\n10.88.0.2 web\n10.88.0.3 api\n10.88.0.4 db\n"
        );
    }

    #[test]
    fn test_hosts_file_own_entry_present() {
        let result = generate_hosts_file("192.168.1.5", "mybox", &[]);
        assert!(result.contains("192.168.1.5 mybox"));
        assert!(result.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn test_hosts_file_deterministic_output() {
        let peers = vec![
            ("10.0.0.2".to_string(), "a".to_string()),
            ("10.0.0.3".to_string(), "b".to_string()),
        ];
        let r1 = generate_hosts_file("10.0.0.1", "self", &peers);
        let r2 = generate_hosts_file("10.0.0.1", "self", &peers);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_hosts_file_with_hostname_without_ip() {
        let result = generate_hosts_file_with_entries(None, &["box1".to_string()], &[], &[]);
        assert_eq!(result, "127.0.0.1 localhost\n127.0.1.1 box1\n");
    }

    #[test]
    fn test_hosts_file_with_extra_hosts() {
        let result = generate_hosts_file_with_entries(
            Some("10.88.0.2"),
            &["web".to_string(), "custom".to_string()],
            &[],
            &[HostEntry {
                host: "db.local".to_string(),
                ip: "10.88.0.10".to_string(),
            }],
        );
        assert_eq!(
            result,
            "127.0.0.1 localhost\n10.88.0.2 web custom\n10.88.0.10 db.local\n"
        );
    }

    #[test]
    fn test_validate_hostname() {
        validate_hostname("web").unwrap();
        validate_hostname("web-1.example").unwrap();
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-web").is_err());
        assert!(validate_hostname("web_1").is_err());
        assert!(validate_hostname("bad host").is_err());
    }

    #[test]
    fn test_parse_add_host_entry() {
        let entry = parse_add_host_entry("db.local:10.88.0.10").unwrap();
        assert_eq!(entry.host, "db.local");
        assert_eq!(entry.ip, "10.88.0.10");

        let entry = parse_add_host_entry("v6:2001:db8::1").unwrap();
        assert_eq!(entry.host, "v6");
        assert_eq!(entry.ip, "2001:db8::1");

        assert!(parse_add_host_entry("missing-ip:").is_err());
        assert!(parse_add_host_entry("bad_host:10.0.0.1").is_err());
        assert!(parse_add_host_entry("host:not-an-ip").is_err());
    }
}
