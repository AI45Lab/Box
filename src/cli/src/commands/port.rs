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
        let mapping = a3s_box_core::parse_port_mapping(mapping)
            .map_err(|e| format!("Invalid persisted port mapping: {e}"))?;
        println!(
            "{}/{} -> 0.0.0.0:{}",
            mapping.guest_port,
            mapping.protocol.as_str(),
            mapping.host_port
        );
    }

    Ok(())
}
