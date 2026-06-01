//! `a3s-box events` command — Stream real-time system events.
//!
//! Monitors state changes and outputs events similar to `docker events`.
//! Events are detected by polling the state file for changes.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::Args;

use crate::state::StateFile;

#[derive(Args)]
pub struct EventsArgs {
    /// Show events since timestamp (RFC3339 or relative like "5m", "1h")
    #[arg(long)]
    pub since: Option<String>,

    /// Show events until timestamp (RFC3339 or relative)
    #[arg(long)]
    pub until: Option<String>,

    /// Filter events (e.g., "type=container", "event=start", "name=mybox")
    #[arg(short, long)]
    pub filter: Vec<String>,

    /// Output in JSON format
    #[arg(long)]
    pub json: bool,
}

/// A single event record.
#[derive(Debug, Clone, serde::Serialize)]
struct Event {
    time: DateTime<Utc>,
    #[serde(rename = "type")]
    event_type: String,
    action: String,
    actor: Actor,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Actor {
    id: String,
    name: String,
    image: String,
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} {} (name={}, image={})",
            self.time.format("%Y-%m-%dT%H:%M:%S%.6fZ"),
            self.event_type,
            self.action,
            &self.actor.id[..12.min(self.actor.id.len())],
            self.actor.name,
            self.actor.image,
        )
    }
}

/// Snapshot of box statuses for change detection.
type StatusSnapshot = HashMap<String, String>;

fn take_snapshot(state: &StateFile) -> StatusSnapshot {
    state
        .list(true)
        .into_iter()
        .map(|r| (r.id.clone(), r.status.clone()))
        .collect()
}

/// Parse a `--since`/`--until` argument: an RFC3339 timestamp, or a relative
/// duration like `30s`, `5m`, `1h`, `2d` interpreted as that long ago.
fn parse_time_arg(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        "d" => n.checked_mul(86400)?,
        _ => return None,
    };
    Some(Utc::now() - chrono::Duration::seconds(secs))
}

fn parse_filters(filters: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for f in filters {
        if let Some((k, v)) = f.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn matches_filters(event: &Event, filters: &HashMap<String, String>) -> bool {
    for (key, value) in filters {
        let matches = match key.as_str() {
            "type" => event.event_type == *value,
            "event" | "action" => event.action == *value,
            "name" => event.actor.name == *value,
            "image" => event.actor.image == *value,
            _ => true,
        };
        if !matches {
            return false;
        }
    }
    true
}

fn status_to_action(old: Option<&str>, new: &str) -> Option<&'static str> {
    match (old, new) {
        (None, "created") => Some("create"),
        (None, "running") => Some("start"),
        (None, "dead") => Some("die"),
        (Some("created"), "running") => Some("start"),
        (Some("running"), "paused") => Some("pause"),
        (Some("paused"), "running") => Some("unpause"),
        (Some("dead"), "running") => Some("restart"),
        (Some(old), "dead") if old != "dead" => Some("die"),
        (Some("running"), "exited") => Some("die"),
        (Some("running"), "stopped") => Some("stop"),
        (Some("exited"), "running") => Some("start"),
        (Some("stopped"), "running") => Some("start"),
        (Some(_), "exited") => Some("die"),
        (Some(_), "stopped") => Some("stop"),
        _ => {
            if old.map(|o| o != new).unwrap_or(false) {
                // Status changed but not a known transition
                Some("update")
            } else {
                None
            }
        }
    }
}

pub async fn execute(args: EventsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let filters = parse_filters(&args.filter);

    // Parse --since/--until (RFC3339 or relative like "30s"/"5m"/"1h").
    let until: Option<DateTime<Utc>> = args.until.as_deref().and_then(parse_time_arg);
    let since: Option<DateTime<Utc>> = args.since.as_deref().and_then(parse_time_arg);

    // A --until already in the past means there is no live window to watch.
    if let Some(deadline) = until {
        if Utc::now() > deadline {
            return Ok(());
        }
    }

    let state = StateFile::load_default()?;
    let mut prev = take_snapshot(&state);

    // Also record full records for actor info
    let mut records: HashMap<String, (String, String)> = state
        .list(true)
        .into_iter()
        .map(|r| (r.id.clone(), (r.name.clone(), r.image.clone())))
        .collect();

    println!("Listening for events... (Ctrl+C to stop)");

    loop {
        // Stop once the --until deadline passes.
        if let Some(deadline) = until {
            if Utc::now() > deadline {
                break;
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let state = StateFile::load_default()?;
        let current = take_snapshot(&state);

        // Update records map
        for r in state.list(true) {
            records
                .entry(r.id.clone())
                .or_insert_with(|| (r.name.clone(), r.image.clone()));
        }

        // Detect new boxes
        for (id, status) in &current {
            let old_status = prev.get(id).map(|s| s.as_str());
            if let Some(action) = status_to_action(old_status, status) {
                let (name, image) = records
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), String::new()));

                let event = Event {
                    time: Utc::now(),
                    event_type: "container".to_string(),
                    action: action.to_string(),
                    actor: Actor {
                        id: id.clone(),
                        name,
                        image,
                    },
                };

                if matches_filters(&event, &filters) && since.is_none_or(|s| event.time >= s) {
                    if args.json {
                        println!("{}", serde_json::to_string(&event)?);
                    } else {
                        println!("{event}");
                    }
                }
            }
        }

        // Detect removed boxes
        for id in prev.keys() {
            if !current.contains_key(id) {
                let (name, image) = records
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), String::new()));

                let event = Event {
                    time: Utc::now(),
                    event_type: "container".to_string(),
                    action: "destroy".to_string(),
                    actor: Actor {
                        id: id.clone(),
                        name,
                        image,
                    },
                };

                if matches_filters(&event, &filters) && since.is_none_or(|s| event.time >= s) {
                    if args.json {
                        println!("{}", serde_json::to_string(&event)?);
                    } else {
                        println!("{event}");
                    }
                }
            }
        }

        prev = current;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_to_action_create() {
        assert_eq!(status_to_action(None, "created"), Some("create"));
    }

    #[test]
    fn test_status_to_action_start() {
        assert_eq!(status_to_action(Some("created"), "running"), Some("start"));
        assert_eq!(status_to_action(None, "running"), Some("start"));
        assert_eq!(status_to_action(Some("exited"), "running"), Some("start"));
        assert_eq!(status_to_action(Some("stopped"), "running"), Some("start"));
    }

    #[test]
    fn test_status_to_action_stop() {
        assert_eq!(status_to_action(Some("running"), "stopped"), Some("stop"));
        assert_eq!(status_to_action(Some("running"), "exited"), Some("die"));
    }

    #[test]
    fn test_status_to_action_dead_transitions() {
        assert_eq!(status_to_action(Some("running"), "dead"), Some("die"));
        assert_eq!(status_to_action(Some("paused"), "dead"), Some("die"));
        assert_eq!(status_to_action(Some("created"), "dead"), Some("die"));
        assert_eq!(status_to_action(None, "dead"), Some("die"));
        assert_eq!(status_to_action(Some("dead"), "running"), Some("restart"));
        assert_eq!(status_to_action(Some("dead"), "dead"), None);
    }

    #[test]
    fn test_status_to_action_pause_unpause() {
        assert_eq!(status_to_action(Some("running"), "paused"), Some("pause"));
        assert_eq!(status_to_action(Some("paused"), "running"), Some("unpause"));
    }

    #[test]
    fn test_status_to_action_no_change() {
        assert_eq!(status_to_action(Some("running"), "running"), None);
    }

    #[test]
    fn test_parse_time_arg_relative_and_rfc3339() {
        // Relative durations resolve to a time in the past.
        assert!(parse_time_arg("1s").unwrap() <= Utc::now());
        assert!(parse_time_arg("5m").is_some());
        assert!(parse_time_arg("2h").is_some());
        assert!(parse_time_arg("1d").is_some());
        assert!(parse_time_arg("30").is_some()); // bare number = seconds
                                                 // RFC3339 parses exactly.
        assert_eq!(
            parse_time_arg("2024-01-15T10:30:00Z").unwrap(),
            chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        // Garbage returns None (does not hang or panic).
        assert!(parse_time_arg("nonsense").is_none());
    }

    #[test]
    fn test_parse_filters() {
        let filters = parse_filters(&["type=container".to_string(), "event=start".to_string()]);
        assert_eq!(filters.get("type").unwrap(), "container");
        assert_eq!(filters.get("event").unwrap(), "start");
    }

    #[test]
    fn test_parse_filters_empty() {
        let filters = parse_filters(&[]);
        assert!(filters.is_empty());
    }

    #[test]
    fn test_matches_filters_all_match() {
        let event = Event {
            time: Utc::now(),
            event_type: "container".to_string(),
            action: "start".to_string(),
            actor: Actor {
                id: "abc123".to_string(),
                name: "mybox".to_string(),
                image: "alpine".to_string(),
            },
        };
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), "container".to_string());
        filters.insert("event".to_string(), "start".to_string());
        assert!(matches_filters(&event, &filters));
    }

    #[test]
    fn test_matches_filters_no_match() {
        let event = Event {
            time: Utc::now(),
            event_type: "container".to_string(),
            action: "stop".to_string(),
            actor: Actor {
                id: "abc123".to_string(),
                name: "mybox".to_string(),
                image: "alpine".to_string(),
            },
        };
        let mut filters = HashMap::new();
        filters.insert("event".to_string(), "start".to_string());
        assert!(!matches_filters(&event, &filters));
    }

    #[test]
    fn test_matches_filters_empty() {
        let event = Event {
            time: Utc::now(),
            event_type: "container".to_string(),
            action: "start".to_string(),
            actor: Actor {
                id: "abc123".to_string(),
                name: "mybox".to_string(),
                image: "alpine".to_string(),
            },
        };
        let filters = HashMap::new();
        assert!(matches_filters(&event, &filters));
    }

    #[test]
    fn test_event_display() {
        let event = Event {
            time: chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            event_type: "container".to_string(),
            action: "start".to_string(),
            actor: Actor {
                id: "abc123def456".to_string(),
                name: "mybox".to_string(),
                image: "alpine:latest".to_string(),
            },
        };
        let s = format!("{event}");
        assert!(s.contains("container"));
        assert!(s.contains("start"));
        assert!(s.contains("abc123def456"));
        assert!(s.contains("mybox"));
        assert!(s.contains("alpine:latest"));
    }

    #[test]
    fn test_matches_filters_by_name() {
        let event = Event {
            time: Utc::now(),
            event_type: "container".to_string(),
            action: "start".to_string(),
            actor: Actor {
                id: "abc".to_string(),
                name: "web".to_string(),
                image: "nginx".to_string(),
            },
        };
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), "web".to_string());
        assert!(matches_filters(&event, &filters));

        filters.insert("name".to_string(), "other".to_string());
        assert!(!matches_filters(&event, &filters));
    }
}
