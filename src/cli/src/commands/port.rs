//! `a3s-box port` command — List port mappings for a box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct PortArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: PortArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.port_map.is_empty() {
        // No port mappings — silent (matches Docker behavior)
        return Ok(());
    }

    for mapping in &record.port_map {
        println!("{}", format_persisted_port_mapping(mapping)?);
    }

    Ok(())
}

fn format_persisted_port_mapping(mapping: &str) -> Result<String, String> {
    let mapping = a3s_box_core::parse_port_mapping(mapping)
        .map_err(|e| format!("Invalid persisted port mapping: {e}"))?;
    Ok(format!(
        "{}/{} -> 0.0.0.0:{}",
        mapping.guest_port,
        mapping.protocol.as_str(),
        mapping.host_port
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_persisted_port_mapping_matches_docker_style_output() {
        let output = format_persisted_port_mapping("18080:80").unwrap();

        assert_eq!(output, "80/tcp -> 0.0.0.0:18080");
    }

    #[test]
    fn format_persisted_port_mapping_accepts_normalized_tcp_suffix() {
        let output = format_persisted_port_mapping("10443:443/tcp").unwrap();

        assert_eq!(output, "443/tcp -> 0.0.0.0:10443");
    }

    #[test]
    fn format_persisted_port_mapping_preserves_auto_host_port_zero() {
        let output = format_persisted_port_mapping("0:8080").unwrap();

        assert_eq!(output, "8080/tcp -> 0.0.0.0:0");
    }

    #[test]
    fn format_persisted_port_mapping_surfaces_corrupt_state() {
        let error = format_persisted_port_mapping("not-a-port").unwrap_err();

        assert!(error.starts_with("Invalid persisted port mapping:"));
        assert!(error.contains("not-a-port"));
    }
}
