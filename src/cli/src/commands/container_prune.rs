//! `a3s-box container prune` command — remove stopped boxes.

use clap::Args;

use crate::state::StateFile;

#[derive(Args)]
pub struct ContainerPruneArgs {
    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: ContainerPruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING: This will remove all stopped boxes.");
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut state = StateFile::load_default()?;
    let to_remove: Vec<String> = state
        .records()
        .iter()
        .filter(|record| is_prunable_status(&record.status))
        .map(|record| record.id.clone())
        .collect();

    let mut removed = 0usize;
    let mut errors = Vec::new();

    for id in &to_remove {
        match super::rm::rm_one(&mut state, id, false) {
            Ok(()) => removed += 1,
            Err(error) => errors.push(format!("{id}: {error}")),
        }
    }

    println!("Total boxes removed: {removed}");

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn is_prunable_status(status: &str) -> bool {
    matches!(status, "created" | "stopped" | "dead")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_prunable_status() {
        assert!(is_prunable_status("created"));
        assert!(is_prunable_status("stopped"));
        assert!(is_prunable_status("dead"));
        assert!(!is_prunable_status("running"));
        assert!(!is_prunable_status("paused"));
    }
}
