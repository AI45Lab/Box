//! `a3s-box stats` command — Display live resource usage statistics.
//!
//! Shows CPU and memory usage for active boxes, similar to `docker stats`.
//! By default streams updates every second; use `--no-stream` for a single snapshot.

use clap::Args;
use sysinfo::{Pid, System};

use crate::output;
use crate::resolve;
use crate::state::{BoxRecord, StateFile};
use crate::status;

#[derive(Args)]
pub struct StatsArgs {
    /// Box name or ID (shows all active boxes if omitted)
    pub r#box: Option<String>,

    /// Disable streaming and print a single snapshot
    #[arg(long)]
    pub no_stream: bool,
}

/// Collected stats for a single box.
struct BoxStats {
    name: String,
    short_id: String,
    status: String,
    pid: u32,
    cpu_percent: f32,
    memory_bytes: u64,
    memory_limit_bytes: u64,
}

/// Collect stats for a process by PID.
///
/// Requires two `refresh_process` calls with a delay between them
/// for accurate CPU measurement (sysinfo computes CPU as a delta).
fn collect_stats(sys: &mut System, pid: u32, memory_limit_mb: u32) -> Option<(f32, u64)> {
    let spid = Pid::from_u32(pid);

    // First refresh to establish baseline
    sys.refresh_process(spid);
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Second refresh to compute CPU delta
    sys.refresh_process(spid);

    sys.process(spid).map(|proc_info| {
        let cpu = proc_info.cpu_usage();
        let mem = proc_info.memory();
        let _ = memory_limit_mb; // used by caller
        (cpu, mem)
    })
}

/// Print a stats table for the given boxes.
fn print_stats(stats: &[BoxStats]) {
    let mut table = output::new_table(&[
        "BOX ID",
        "NAME",
        "STATUS",
        "CPU %",
        "MEM USAGE / LIMIT",
        "MEM %",
        "PID",
    ]);

    for s in stats {
        let mem_pct = if s.memory_limit_bytes > 0 {
            (s.memory_bytes as f64 / s.memory_limit_bytes as f64) * 100.0
        } else {
            0.0
        };

        table.add_row([
            &s.short_id,
            &s.name,
            &s.status,
            &format!("{:.2}%", s.cpu_percent),
            &format!(
                "{} / {}",
                output::format_bytes(s.memory_bytes),
                output::format_bytes(s.memory_limit_bytes)
            ),
            &format!("{:.1}%", mem_pct),
            &s.pid.to_string(),
        ]);
    }

    println!("{table}");
}

fn select_targets(
    state: &StateFile,
    query: Option<&str>,
) -> Result<Vec<BoxRecord>, Box<dyn std::error::Error>> {
    if let Some(name) = query {
        let record = resolve::resolve(state, name)?;
        status::require_active(record, "show stats for")
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        return Ok(vec![record.clone()]);
    }

    Ok(state
        .list(true)
        .into_iter()
        .filter(|record| status::is_active(record))
        .cloned()
        .collect())
}

fn build_box_stats(sys: &mut System, record: &BoxRecord) -> Option<BoxStats> {
    let pid = record.pid?;
    let memory_limit_bytes = (record.memory_mb as u64) * 1024 * 1024;
    collect_stats(sys, pid, record.memory_mb).map(|(cpu, mem)| BoxStats {
        name: record.name.clone(),
        short_id: record.short_id.clone(),
        status: record.status.clone(),
        pid,
        cpu_percent: cpu,
        memory_bytes: mem,
        memory_limit_bytes,
    })
}

pub async fn execute(args: StatsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut sys = System::new();

    loop {
        let state = StateFile::load_default()?;

        // Determine which boxes to show
        let targets = select_targets(&state, args.r#box.as_deref())?;

        if targets.is_empty() {
            println!("No active boxes");
            return Ok(());
        }

        // Collect stats for each active box.
        let mut stats = Vec::new();
        for record in &targets {
            if let Some(box_stats) = build_box_stats(&mut sys, record) {
                stats.push(box_stats);
            }
        }

        // Clear screen for streaming mode (except first iteration)
        if !args.no_stream {
            // Use ANSI escape to move cursor to top and clear
            print!("\x1B[2J\x1B[H");
        }

        print_stats(&stats);

        if args.no_stream {
            break;
        }

        // Wait before next refresh
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_select_targets_without_query_includes_running_and_paused() {
        let (_tmp, state) = setup_state(vec![
            make_record("id-1", "running_box", "running", Some(1)),
            make_record("id-2", "paused_box", "paused", Some(1)),
            make_record("id-3", "stopped_box", "stopped", None),
        ]);

        let targets = select_targets(&state, None).unwrap();
        let names: Vec<_> = targets.iter().map(|record| record.name.as_str()).collect();

        assert_eq!(names, vec!["running_box", "paused_box"]);
    }

    #[test]
    fn test_select_targets_with_query_accepts_paused() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "paused_box", "paused", Some(1))]);

        let targets = select_targets(&state, Some("paused_box")).unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].status, "paused");
    }

    #[test]
    fn test_select_targets_with_query_rejects_stopped() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "stopped_box", "stopped", None)]);

        let error = select_targets(&state, Some("stopped_box")).unwrap_err();

        assert!(error.to_string().contains("Cannot show stats for"));
        assert!(error.to_string().contains("a3s-box start stopped_box"));
    }
}
