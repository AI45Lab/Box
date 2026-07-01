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

    let rootfs_dir = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
        format!(
            "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/). \
             For overlay-backed boxes the filesystem is only available while the box exists; \
             export a running box.",
            args.name,
            record.box_dir.display()
        )
    })?;

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

    println!(
        "Exported {} to {} ({})",
        args.name,
        args.output,
        crate::output::format_bytes(size)
    );
    Ok(())
}
