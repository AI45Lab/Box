//! `a3s-box export` command — Export a box's filesystem to a tar archive.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ExportArgs {
    /// Box name or ID to export
    pub name: String,

    /// Output file path (e.g., "mybox.tar")
    #[arg(short, long)]
    pub output: String,
}

pub async fn execute(args: ExportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.name)?;

    let rootfs_dir = super::resolve_box_rootfs(&record.box_dir)
        .ok_or_else(|| rootfs_not_found_message(&args.name, &record.box_dir))?;

    let file = std::fs::File::create(&args.output)
        .map_err(|e| format!("Failed to create {}: {e}", args.output))?;

    let mut builder = tar::Builder::new(file);
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", &rootfs_dir)
        .map_err(|e| format!("Failed to archive filesystem: {e}"))?;
    builder
        .finish()
        .map_err(|e| format!("Failed to finalize archive: {e}"))?;

    let size = std::fs::metadata(&args.output)
        .map(|m| m.len())
        .unwrap_or(0);

    println!("{}", export_success_line(&args.name, &args.output, size));
    Ok(())
}

fn rootfs_not_found_message(name: &str, box_dir: &std::path::Path) -> String {
    format!(
        "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/). \
         For overlay-backed boxes the filesystem is only available while the box exists; \
         export a running box.",
        name,
        box_dir.display()
    )
}

fn export_success_line(name: &str, output: &str, size: u64) -> String {
    format!(
        "Exported {} to {} ({})",
        name,
        output,
        crate::output::format_bytes(size)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rootfs_not_found_message_mentions_box_path_and_expected_dirs() {
        let message = rootfs_not_found_message("web", Path::new("/tmp/a3s/boxes/web"));

        assert!(message.contains("Rootfs not found for box 'web'"));
        assert!(message.contains("/tmp/a3s/boxes/web"));
        assert!(message.contains("merged/ and rootfs/"));
        assert!(message.contains("export a running box"));
    }

    #[test]
    fn export_success_line_formats_archive_size() {
        assert_eq!(
            export_success_line("web", "web.tar", 1536),
            "Exported web to web.tar (1.5 KB)"
        );
    }
}
