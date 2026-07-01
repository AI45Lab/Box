//! `a3s-box stats` command — Display live resource usage statistics.
//!
//! Shows CPU, memory, network, and block I/O usage for active boxes, similar to `docker stats`.
//! By default streams updates every second; use `--no-stream` for a single snapshot.

use clap::{Args, ValueEnum};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;
use sysinfo::{Pid, System};

#[cfg(not(windows))]
use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
#[cfg(not(windows))]
use a3s_box_runtime::ExecClient;

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

    /// Output format
    #[arg(long, value_enum, default_value_t = StatsFormat::Table)]
    format: StatsFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum StatsFormat {
    Table,
    Json,
}

const PIDS_CURRENT_TIMEOUT: Duration = Duration::from_millis(750);

/// Collected stats for a single box.
#[derive(Serialize)]
struct BoxStats {
    id: String,
    name: String,
    short_id: String,
    status: String,
    pid: u32,
    cpus: u32,
    cpu_percent: f32,
    memory_bytes: u64,
    memory_limit_bytes: u64,
    network_rx_bytes: u64,
    network_tx_bytes: u64,
    block_read_bytes: u64,
    block_write_bytes: u64,
    pids_current: Option<u64>,
}

impl BoxStats {
    fn mem_percent(&self) -> f64 {
        if self.memory_limit_bytes > 0 {
            (self.memory_bytes as f64 / self.memory_limit_bytes as f64) * 100.0
        } else {
            0.0
        }
    }

    fn scaled_cpu_percent(&self) -> f64 {
        self.cpu_percent as f64 / self.cpus.max(1) as f64
    }
}

struct ResourceStats {
    cpu_percent: f32,
    memory_bytes: u64,
    block_read_bytes: u64,
    block_write_bytes: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NetworkStats {
    rx_bytes: u64,
    tx_bytes: u64,
}

/// Collect stats for a process by PID.
///
/// Requires two `refresh_process` calls with a delay between them
/// for accurate CPU measurement (sysinfo computes CPU as a delta).
fn collect_stats(sys: &mut System, pid: u32) -> Option<ResourceStats> {
    let spid = Pid::from_u32(pid);

    // First refresh to establish baseline
    sys.refresh_process(spid);
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Second refresh to compute CPU delta
    sys.refresh_process(spid);

    sys.process(spid).map(|proc_info| {
        let disk = proc_info.disk_usage();
        ResourceStats {
            cpu_percent: proc_info.cpu_usage(),
            memory_bytes: proc_info.memory(),
            block_read_bytes: disk.total_read_bytes,
            block_write_bytes: disk.total_written_bytes,
        }
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
        "NET I/O",
        "IO",
    ]);

    for s in stats {
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
            &format!("{:.1}%", s.mem_percent()),
            &s.pid.to_string(),
            &format_io_usage(s.network_rx_bytes, s.network_tx_bytes),
            &format_io_usage(s.block_read_bytes, s.block_write_bytes),
        ]);
    }

    println!("{table}");
}

fn print_stats_json(stats: &[BoxStats]) -> Result<(), serde_json::Error> {
    let rows = stats.iter().map(stats_json).collect::<Vec<_>>();
    println!("{}", serde_json::to_string(&rows)?);
    Ok(())
}

fn stats_json(stats: &BoxStats) -> serde_json::Value {
    serde_json::json!({
        "id": stats.id,
        "short_id": stats.short_id,
        "name": stats.name,
        "status": stats.status,
        "pid": stats.pid,
        "cpus": stats.cpus,
        "cpu_count": stats.cpus,
        "cpu_percent": stats.cpu_percent,
        "cpu_percent_scaled": stats.scaled_cpu_percent(),
        "memory_bytes": stats.memory_bytes,
        "memory_limit_bytes": stats.memory_limit_bytes,
        "memory_percent": stats.mem_percent(),
        "network_rx_bytes": stats.network_rx_bytes,
        "network_tx_bytes": stats.network_tx_bytes,
        "block_read_bytes": stats.block_read_bytes,
        "block_write_bytes": stats.block_write_bytes,
        "pids_current": stats.pids_current,
        "pids": {
            "current": stats.pids_current,
        },
    })
}

fn format_io_usage(read_bytes: u64, write_bytes: u64) -> String {
    format!(
        "{} / {}",
        output::format_bytes(read_bytes),
        output::format_bytes(write_bytes)
    )
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
    let network = collect_network_stats(record);
    collect_stats(sys, pid).map(|stats| BoxStats {
        id: record.id.clone(),
        name: record.name.clone(),
        short_id: record.short_id.clone(),
        status: record.status.clone(),
        pid,
        cpus: record.cpus,
        cpu_percent: stats.cpu_percent,
        memory_bytes: stats.memory_bytes,
        memory_limit_bytes,
        network_rx_bytes: network.rx_bytes,
        network_tx_bytes: network.tx_bytes,
        block_read_bytes: stats.block_read_bytes,
        block_write_bytes: stats.block_write_bytes,
        pids_current: None,
    })
}

async fn collect_pids_current(record: &BoxRecord) -> Option<u64> {
    #[cfg(not(windows))]
    {
        tokio::time::timeout(PIDS_CURRENT_TIMEOUT, collect_guest_process_count(record))
            .await
            .ok()
            .flatten()
    }
    #[cfg(windows)]
    {
        let _ = record;
        None
    }
}

#[cfg(not(windows))]
async fn collect_guest_process_count(record: &BoxRecord) -> Option<u64> {
    let exec_socket_path =
        crate::socket_paths::runtime_socket(record, crate::socket_paths::RuntimeSocket::Exec);
    let client = ExecClient::connect(&exec_socket_path).await.ok()?;
    let request = ExecRequest {
        cmd: vec!["ps".to_string(), "-eo".to_string(), "pid,args".to_string()],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };
    let output = client.exec_command(&request).await.ok()?;
    if output.exit_code != 0 {
        return None;
    }
    Some(count_guest_processes(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(not(windows))]
fn count_guest_processes(text: &str) -> u64 {
    text.lines()
        .filter_map(parse_guest_process_command)
        .filter(|command| !is_process_count_probe(command))
        .count() as u64
}

#[cfg(not(windows))]
fn parse_guest_process_command(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let pid = parts.next()?;
    pid.parse::<u32>().ok()?;
    Some(parts.next().unwrap_or("").trim())
}

#[cfg(not(windows))]
fn is_process_count_probe(command: &str) -> bool {
    let command = command.trim();
    command == "ps"
        || command.starts_with("ps -eo pid,args")
        || command.starts_with("/bin/ps -eo pid,args")
        || command.starts_with("/usr/bin/ps -eo pid,args")
}

fn collect_network_stats(record: &BoxRecord) -> NetworkStats {
    read_network_stats_file(&record.box_dir.join("sockets").join("net.stats.json"))
        .or_else(|| collect_passt_pcap_stats(record))
        .unwrap_or_default()
}

fn read_network_stats_file(path: &Path) -> Option<NetworkStats> {
    let data = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    Some(NetworkStats {
        rx_bytes: json.get("rx_bytes")?.as_u64()?,
        tx_bytes: json.get("tx_bytes")?.as_u64()?,
    })
}

fn collect_passt_pcap_stats(record: &BoxRecord) -> Option<NetworkStats> {
    let pcap_path = record.exec_socket_path.parent()?.join("passt.pcap");
    let guest_mac = guest_mac_address(record)?;
    read_passt_pcap_stats(&pcap_path, guest_mac)
}

fn guest_mac_address(record: &BoxRecord) -> Option<[u8; 6]> {
    let network_name = crate::cleanup::record_network_name(record)?;
    let store = a3s_box_runtime::NetworkStore::default_path().ok()?;
    let network = store.get(network_name).ok()??;
    let endpoint = network.endpoints.get(&record.id)?;
    parse_mac_address(&endpoint.mac_address)
}

fn parse_mac_address(value: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = value.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

fn read_passt_pcap_stats(path: &Path, guest_mac: [u8; 6]) -> Option<NetworkStats> {
    let data = std::fs::read(path).ok()?;
    parse_passt_pcap_stats(&data, guest_mac)
}

fn parse_passt_pcap_stats(data: &[u8], guest_mac: [u8; 6]) -> Option<NetworkStats> {
    let endian = PcapEndian::from_magic(data.get(..4)?)?;
    if data.len() < 24 {
        return None;
    }

    let mut offset = 24;
    let mut stats = NetworkStats::default();
    while offset + 16 <= data.len() {
        let incl_len = endian.read_u32(data.get(offset + 8..offset + 12)?) as usize;
        let orig_len = endian.read_u32(data.get(offset + 12..offset + 16)?) as u64;
        offset += 16;
        if offset + incl_len > data.len() {
            break;
        }

        let frame = &data[offset..offset + incl_len];
        if frame.len() >= 14 {
            let frame_len = if orig_len == 0 {
                incl_len as u64
            } else {
                orig_len
            };
            if frame[6..12] == guest_mac {
                stats.tx_bytes = stats.tx_bytes.saturating_add(frame_len);
            } else if frame[0..6] == guest_mac || frame[0..6] == [0xff; 6] {
                stats.rx_bytes = stats.rx_bytes.saturating_add(frame_len);
            }
        }

        offset += incl_len;
    }

    Some(stats)
}

#[derive(Clone, Copy)]
enum PcapEndian {
    Little,
    Big,
}

impl PcapEndian {
    fn from_magic(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => Some(Self::Little),
            [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => Some(Self::Big),
            _ => None,
        }
    }

    fn read_u32(self, bytes: &[u8]) -> u32 {
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&bytes[..4]);
        match self {
            Self::Little => u32::from_le_bytes(arr),
            Self::Big => u32::from_be_bytes(arr),
        }
    }
}

pub async fn execute(args: StatsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut sys = System::new();

    loop {
        let state = StateFile::load_default()?;

        // Determine which boxes to show
        let targets = select_targets(&state, args.r#box.as_deref())?;

        if targets.is_empty() {
            match args.format {
                StatsFormat::Table => println!("No active boxes"),
                StatsFormat::Json => println!("[]"),
            }
            return Ok(());
        }

        // Collect stats for each active box.
        let mut stats = Vec::new();
        for record in &targets {
            if let Some(mut box_stats) = build_box_stats(&mut sys, record) {
                if args.format == StatsFormat::Json {
                    box_stats.pids_current = collect_pids_current(record).await;
                }
                stats.push(box_stats);
            }
        }

        match args.format {
            StatsFormat::Table => {
                // Clear screen for streaming mode (except first iteration)
                if !args.no_stream {
                    // Use ANSI escape to move cursor to top and clear
                    print!("\x1B[2J\x1B[H");
                }
                print_stats(&stats);
            }
            StatsFormat::Json => print_stats_json(&stats)?,
        }

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

    #[test]
    fn test_format_io_usage_matches_stats_table_pair() {
        assert_eq!(format_io_usage(1024, 2 * 1024 * 1024), "1.0 KB / 2.0 MB");
    }

    #[test]
    fn test_read_network_stats_file_reads_netproxy_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("net.stats.json");
        std::fs::write(
            &path,
            r#"{"schema":"a3s-box.netproxy.stats.v1","rx_bytes":1024,"tx_bytes":2048,"rx_packets":3,"tx_packets":4}"#,
        )
        .unwrap();

        let stats = read_network_stats_file(&path).unwrap();

        assert_eq!(
            stats,
            NetworkStats {
                rx_bytes: 1024,
                tx_bytes: 2048
            }
        );
    }

    #[test]
    fn test_stats_json_exposes_machine_readable_fields() {
        let row = BoxStats {
            id: "abcdef1234567890".to_string(),
            name: "dev".to_string(),
            short_id: "abc123".to_string(),
            status: "running".to_string(),
            pid: 4242,
            cpus: 2,
            cpu_percent: 12.5,
            memory_bytes: 64 * 1024 * 1024,
            memory_limit_bytes: 512 * 1024 * 1024,
            network_rx_bytes: 1024,
            network_tx_bytes: 2048,
            block_read_bytes: 4096,
            block_write_bytes: 8192,
            pids_current: Some(7),
        };

        let json = stats_json(&row);

        assert_eq!(json["id"], "abcdef1234567890");
        assert_eq!(json["short_id"], "abc123");
        assert_eq!(json["name"], "dev");
        assert_eq!(json["pid"], 4242);
        assert_eq!(json["cpus"], 2);
        assert_eq!(json["cpu_count"], 2);
        assert_eq!(json["cpu_percent"], 12.5);
        assert_eq!(json["cpu_percent_scaled"], 6.25);
        assert_eq!(json["memory_bytes"], 64 * 1024 * 1024);
        assert_eq!(json["memory_limit_bytes"], 512 * 1024 * 1024);
        assert_eq!(json["memory_percent"], 12.5);
        assert_eq!(json["network_rx_bytes"], 1024);
        assert_eq!(json["network_tx_bytes"], 2048);
        assert_eq!(json["block_read_bytes"], 4096);
        assert_eq!(json["block_write_bytes"], 8192);
        assert_eq!(json["pids_current"], 7);
        assert_eq!(json["pids"]["current"], 7);
    }

    #[cfg(not(windows))]
    #[test]
    fn test_count_guest_processes_ignores_probe_process() {
        let count = count_guest_processes(
            "PID COMMAND\n1 /sbin/init\n7 worker --serve\n9 ps -eo pid,args\n",
        );

        assert_eq!(count, 2);
    }

    #[test]
    fn test_parse_mac_address() {
        assert_eq!(
            parse_mac_address("02:42:0a:58:00:02"),
            Some([0x02, 0x42, 0x0a, 0x58, 0x00, 0x02])
        );
        assert_eq!(parse_mac_address("02:42:0a:58:00"), None);
        assert_eq!(parse_mac_address("02:42:0a:58:00:zz"), None);
    }

    #[test]
    fn test_parse_passt_pcap_stats_classifies_guest_rx_tx() {
        let guest = [0x02, 0x42, 0x0a, 0x58, 0x00, 0x02];
        let gateway = [0x9a, 0x55, 0x9a, 0x55, 0x9a, 0x55];
        let other = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let data = little_endian_pcap(&[
            ethernet_frame(gateway, guest, 60),
            ethernet_frame(guest, gateway, 74),
            ethernet_frame([0xff; 6], other, 90),
        ]);

        let stats = parse_passt_pcap_stats(&data, guest).unwrap();

        assert_eq!(
            stats,
            NetworkStats {
                rx_bytes: 164,
                tx_bytes: 60
            }
        );
    }

    #[test]
    fn test_parse_passt_pcap_stats_ignores_truncated_tail() {
        let guest = [0x02, 0x42, 0x0a, 0x58, 0x00, 0x02];
        let gateway = [0x9a, 0x55, 0x9a, 0x55, 0x9a, 0x55];
        let mut data = little_endian_pcap(&[ethernet_frame(guest, gateway, 64)]);
        data.extend_from_slice(&[0, 0, 0, 0, 10, 0, 0]);

        let stats = parse_passt_pcap_stats(&data, guest).unwrap();

        assert_eq!(
            stats,
            NetworkStats {
                rx_bytes: 64,
                tx_bytes: 0
            }
        );
    }

    fn little_endian_pcap(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&65535u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());

        for frame in frames {
            data.extend_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            data.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            data.extend_from_slice(frame);
        }

        data
    }

    fn ethernet_frame(dst: [u8; 6], src: [u8; 6], len: usize) -> Vec<u8> {
        assert!(len >= 14);
        let mut frame = vec![0u8; len];
        frame[..6].copy_from_slice(&dst);
        frame[6..12].copy_from_slice(&src);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame
    }
}
