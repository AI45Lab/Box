//! `a3s-box ps` command — List boxes.

use clap::Args;

use crate::output;
use crate::state::{BoxRecord, StateFile};
use crate::status;

#[derive(Args)]
pub struct PsArgs {
    /// Show all boxes (including stopped)
    #[arg(short, long)]
    pub all: bool,

    /// Only display box IDs
    #[arg(short, long)]
    pub quiet: bool,

    /// Format output using placeholders: {{.ID}}, {{.Image}}, {{.Status}},
    /// {{.Created}}, {{.Names}}, {{.Ports}}, {{.Command}}
    #[arg(long)]
    pub format: Option<String>,

    /// Filter boxes (e.g., status=running, name=dev, ancestor=alpine)
    #[arg(short, long = "filter")]
    pub filters: Vec<String>,
}

pub async fn execute(args: PsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = select_records(&state, args.all);

    // Apply filters
    let boxes: Vec<&BoxRecord> = boxes
        .into_iter()
        .filter(|r| matches_filters(r, &args.filters))
        .collect();

    // --quiet: print only IDs
    if args.quiet {
        for record in &boxes {
            println!("{}", record.short_id);
        }
        return Ok(());
    }

    // --format: custom template output
    if let Some(ref fmt) = args.format {
        for record in &boxes {
            println!("{}", apply_format(record, fmt));
        }
        return Ok(());
    }

    // Default: table output
    let mut table = output::new_table(&["BOX ID", "IMAGE", "STATUS", "CREATED", "PORTS", "NAMES"]);

    for record in boxes {
        let ports = record.port_map.join(", ");
        let status = status::format_status(record);
        table.add_row([
            &record.short_id,
            &record.image,
            &status,
            &output::format_ago(&record.created_at),
            &ports,
            &record.name,
        ]);
    }

    println!("{table}");
    Ok(())
}

/// Check if a box record matches all the given filters.
///
/// Supported filters:
/// - `status=<value>` — match box status (created, running, paused, stopped, dead)
/// - `name=<value>` — match box name (substring)
/// - `ancestor=<value>` — match image reference (substring)
/// - `id=<value>` — match box ID prefix
fn matches_filters(record: &BoxRecord, filters: &[String]) -> bool {
    for filter in filters {
        let (key, value) = match filter.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };

        let matched = match key {
            "status" => record.status == value,
            "name" => record.name.contains(value),
            "ancestor" => record.image.contains(value),
            "id" => record.id.starts_with(value) || record.short_id.starts_with(value),
            "label" => match_label(&record.labels, value),
            _ => true, // Ignore unknown filters
        };

        if !matched {
            return false;
        }
    }
    true
}

fn select_records(state: &StateFile, all: bool) -> Vec<&BoxRecord> {
    let records = state.list(true);
    if all {
        return records;
    }

    records
        .into_iter()
        .filter(|record| status::is_default_ps_visible(record))
        .collect()
}

/// Apply a format template, replacing `{{.Field}}` placeholders.
fn apply_format(record: &BoxRecord, fmt: &str) -> String {
    let labels_str = format_labels(&record.labels);
    let status = status::format_status(record);
    fmt.replace("{{.ID}}", &record.short_id)
        .replace("{{.Image}}", &record.image)
        .replace("{{.Status}}", &status)
        .replace("{{.Created}}", &output::format_ago(&record.created_at))
        .replace("{{.Names}}", &record.name)
        .replace("{{.Command}}", &record.cmd.join(" "))
        .replace("{{.Ports}}", &record.port_map.join(", "))
        .replace("{{.Labels}}", &labels_str)
}

/// Check if a box's labels match a label filter value.
///
/// Supports two forms:
/// - `label=key` — check if the label key exists
/// - `label=key=value` — check if the label key has the exact value
fn match_label(labels: &std::collections::HashMap<String, String>, filter_value: &str) -> bool {
    if let Some((key, value)) = filter_value.split_once('=') {
        labels.get(key).is_some_and(|v| v == value)
    } else {
        labels.contains_key(filter_value)
    }
}

/// Format labels as a comma-separated "key=value" string.
fn format_labels(labels: &std::collections::HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_record(name: &str, status: &str, labels: HashMap<String, String>) -> BoxRecord {
        let id = format!("test-id-{name}");
        let short_id = BoxRecord::make_short_id(&id);
        BoxRecord {
            id: id.clone(),
            short_id,
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            status: status.to_string(),
            pid: None,
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::from("/tmp").join(&id),
            exec_socket_path: PathBuf::from("/tmp")
                .join(&id)
                .join("sockets")
                .join("exec.sock"),
            console_log: PathBuf::from("/tmp").join(&id).join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: None,
            restart_policy: "no".to_string(),
            port_map: vec![],
            labels,
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: None,
            healthcheck_disabled: false,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: a3s_box_core::NetworkMode::default(),
            network_name: None,
            volume_names: vec![],
            tmpfs: vec![],
            anonymous_volumes: vec![],
            resource_limits: a3s_box_core::config::ResourceLimits::default(),
            log_config: a3s_box_core::log::LogConfig::default(),
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        }
    }

    // --- match_label tests ---

    #[test]
    fn test_match_label_key_exists() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        assert!(match_label(&labels, "env"));
    }

    #[test]
    fn test_match_label_key_not_exists() {
        let labels = HashMap::new();
        assert!(!match_label(&labels, "env"));
    }

    #[test]
    fn test_match_label_key_value_match() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        assert!(match_label(&labels, "env=prod"));
    }

    #[test]
    fn test_match_label_key_value_mismatch() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        assert!(!match_label(&labels, "env=staging"));
    }

    #[test]
    fn test_match_label_key_value_key_missing() {
        let labels = HashMap::new();
        assert!(!match_label(&labels, "env=prod"));
    }

    // --- format_labels tests ---

    #[test]
    fn test_format_labels_empty() {
        let labels = HashMap::new();
        assert_eq!(format_labels(&labels), "");
    }

    #[test]
    fn test_format_labels_single() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        assert_eq!(format_labels(&labels), "env=prod");
    }

    #[test]
    fn test_format_labels_multiple_sorted() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        labels.insert("app".to_string(), "web".to_string());
        assert_eq!(format_labels(&labels), "app=web,env=prod");
    }

    // --- matches_filters with label tests ---

    #[test]
    fn test_filter_label_key_only() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let record = make_record("box1", "running", labels);
        assert!(matches_filters(&record, &["label=env".to_string()]));
    }

    #[test]
    fn test_filter_label_key_value() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let record = make_record("box1", "running", labels);
        assert!(matches_filters(&record, &["label=env=prod".to_string()]));
        assert!(!matches_filters(&record, &["label=env=dev".to_string()]));
    }

    #[test]
    fn test_filter_label_no_labels() {
        let record = make_record("box1", "running", HashMap::new());
        assert!(!matches_filters(&record, &["label=env".to_string()]));
    }

    #[test]
    fn test_filter_combined_status_and_label() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let record = make_record("box1", "running", labels);
        assert!(matches_filters(
            &record,
            &["status=running".to_string(), "label=env".to_string()]
        ));
        assert!(!matches_filters(
            &record,
            &["status=stopped".to_string(), "label=env".to_string()]
        ));
    }

    // --- apply_format with labels ---

    #[test]
    fn test_apply_format_labels() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let record = make_record("box1", "running", labels);
        let result = apply_format(&record, "{{.Names}} {{.Labels}}");
        assert!(result.contains("box1"));
        assert!(result.contains("env=prod"));
    }

    #[test]
    fn test_apply_format_labels_empty() {
        let record = make_record("box1", "running", HashMap::new());
        let result = apply_format(&record, "{{.Labels}}");
        assert_eq!(result, "");
    }

    // --- existing filter tests ---

    #[test]
    fn test_filter_status() {
        let record = make_record("box1", "running", HashMap::new());
        assert!(matches_filters(&record, &["status=running".to_string()]));
        assert!(!matches_filters(&record, &["status=stopped".to_string()]));
    }

    #[test]
    fn test_filter_status_paused() {
        let record = make_record("box1", "paused", HashMap::new());
        assert!(matches_filters(&record, &["status=paused".to_string()]));
        assert!(!matches_filters(&record, &["status=running".to_string()]));
    }

    #[test]
    fn test_filter_name() {
        let record = make_record("my_box", "running", HashMap::new());
        assert!(matches_filters(&record, &["name=my".to_string()]));
        assert!(!matches_filters(&record, &["name=other".to_string()]));
    }

    #[test]
    fn test_filter_ancestor() {
        let record = make_record("box1", "running", HashMap::new());
        assert!(matches_filters(&record, &["ancestor=alpine".to_string()]));
        assert!(!matches_filters(&record, &["ancestor=ubuntu".to_string()]));
    }

    #[test]
    fn test_filter_no_filters() {
        let record = make_record("box1", "running", HashMap::new());
        assert!(matches_filters(&record, &[]));
    }

    #[test]
    fn test_filter_unknown_key_ignored() {
        let record = make_record("box1", "running", HashMap::new());
        assert!(matches_filters(&record, &["unknown=value".to_string()]));
    }

    // --- format_status tests ---

    #[test]
    fn test_format_status_no_health_check() {
        let record = make_record("box1", "running", HashMap::new());
        assert_eq!(status::format_status(&record), "running");
    }

    #[test]
    fn test_format_status_with_health_healthy() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });
        record.health_status = "healthy".to_string();
        assert_eq!(status::format_status(&record), "running (healthy)");
    }

    #[test]
    fn test_format_status_with_health_unhealthy() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["false".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });
        record.health_status = "unhealthy".to_string();
        assert_eq!(status::format_status(&record), "running (unhealthy)");
    }

    #[test]
    fn test_format_status_with_health_starting() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });
        record.health_status = "starting".to_string();
        assert_eq!(status::format_status(&record), "running (starting)");
    }

    #[test]
    fn test_format_status_health_none_not_shown() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });
        record.health_status = "none".to_string();
        // "none" should not be shown
        assert_eq!(status::format_status(&record), "running");
    }

    // --- Restart count in status tests ---

    #[test]
    fn test_format_status_with_restart_count() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.restart_count = 3;
        assert_eq!(status::format_status(&record), "running (Restarts: 3)");
    }

    #[test]
    fn test_format_status_restart_count_zero_not_shown() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.restart_count = 0;
        assert_eq!(status::format_status(&record), "running");
    }

    #[test]
    fn test_format_status_health_and_restart_count() {
        let mut record = make_record("box1", "running", HashMap::new());
        record.health_check = Some(crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });
        record.health_status = "healthy".to_string();
        record.restart_count = 2;
        assert_eq!(
            status::format_status(&record),
            "running (healthy, Restarts: 2)"
        );
    }
}
