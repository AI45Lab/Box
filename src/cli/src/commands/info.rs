//! `a3s-box info` command.

use clap::Args;

use crate::state::BoxRecord;
use crate::state::StateFile;
use crate::status;

use super::images_dir;

#[derive(Args)]
pub struct InfoArgs;

pub async fn execute(_args: InfoArgs) -> Result<(), Box<dyn std::error::Error>> {
    println!("a3s-box version {}", a3s_box_core::VERSION);

    // Virtualization support
    match a3s_box_runtime::check_virtualization_support() {
        Ok(support) => {
            println!("Virtualization: {} ({})", support.backend, support.details);
        }
        Err(e) => {
            println!("Virtualization: not available ({e})");
        }
    }

    // Home directory
    let home = a3s_box_core::dirs_home();
    println!("Home directory: {}", home.display());

    // Box count
    match StateFile::load_default() {
        Ok(state) => {
            let counts = box_counts(&state);
            println!(
                "Boxes: {} total, {} active ({} running, {} paused)",
                counts.total, counts.active, counts.running, counts.paused
            );
        }
        Err(_) => {
            println!("Boxes: 0 total, 0 active (0 running, 0 paused)");
        }
    }

    // Image cache stats
    let images_dir = images_dir();
    if images_dir.exists() {
        match super::open_image_store() {
            Ok(store) => {
                let images = store.list().await;
                let total_size: u64 = images.iter().map(|i| i.size_bytes).sum();
                println!(
                    "Images: {} cached ({})",
                    images.len(),
                    crate::output::format_bytes(total_size)
                );
            }
            Err(_) => {
                println!("Images: 0 cached");
            }
        }
    } else {
        println!("Images: 0 cached");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxCounts {
    total: usize,
    active: usize,
    running: usize,
    paused: usize,
}

fn box_counts(state: &StateFile) -> BoxCounts {
    box_counts_from_records(state.list(true))
}

fn box_counts_from_records(records: Vec<&BoxRecord>) -> BoxCounts {
    let total = records.len();
    let running = records
        .iter()
        .filter(|record| record.status == "running")
        .count();
    let paused = records
        .iter()
        .filter(|record| record.status == "paused")
        .count();
    let active = records
        .iter()
        .filter(|record| status::is_active(record))
        .count();

    BoxCounts {
        total,
        active,
        running,
        paused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_box_counts_include_paused_as_active() {
        let (_tmp, state) = setup_state(vec![
            make_record("id-1", "running", "running", Some(1)),
            make_record("id-2", "paused", "paused", Some(1)),
            make_record("id-3", "created", "created", None),
            make_record("id-4", "stopped", "stopped", None),
            make_record("id-5", "dead", "dead", None),
        ]);

        assert_eq!(
            box_counts(&state),
            BoxCounts {
                total: 5,
                active: 2,
                running: 1,
                paused: 1,
            }
        );
    }
}
