//! `a3s-box inspect` command — Detailed box information as JSON.

use clap::Args;
use serde::Serialize;

use crate::resolve::{self, ResolveError};
use crate::state::{BoxRecord, StateFile};
use crate::status;

use super::image_inspect;

#[derive(Args)]
pub struct InspectArgs {
    /// Container or image name/ID
    pub r#box: String,
}

pub async fn execute(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;

    // `docker inspect` is polymorphic: try a container first, then fall back to
    // an image so `inspect <image>` works the same as `inspect <container>`.
    match resolve::resolve(&state, &args.r#box) {
        Ok(record) => {
            println!("{}", inspect_json(record)?);
            Ok(())
        }
        Err(ResolveError::NotFound(_)) => {
            match image_inspect::try_image_inspect_json(&args.r#box).await? {
                Some(json) => {
                    println!("{json}");
                    Ok(())
                }
                None => Err(format!("No such container or image: {}", args.r#box).into()),
            }
        }
        Err(other) => Err(other.into()),
    }
}

/// Docker-shaped `State` sub-object so tooling can read `.[0].State.Running` etc.
#[derive(Serialize)]
struct DockerState {
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "Paused")]
    paused: bool,
    #[serde(rename = "ExitCode")]
    exit_code: i32,
}

#[derive(Serialize)]
struct InspectView<'a> {
    #[serde(flatten)]
    record: &'a BoxRecord,
    status_detail: status::StatusDetails,
    #[serde(rename = "State")]
    state: DockerState,
}

fn inspect_json(record: &BoxRecord) -> Result<String, serde_json::Error> {
    let view = InspectView {
        record,
        status_detail: status::status_details(record),
        state: DockerState {
            status: record.status.clone(),
            // Docker: a paused container is still Running (Running=true, Paused=true).
            running: matches!(record.status.as_str(), "running" | "paused"),
            paused: record.status == "paused",
            exit_code: record.exit_code.unwrap_or(0),
        },
    };
    // `docker inspect` returns a top-level JSON array, even for one container.
    serde_json::to_string_pretty(&vec![view])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_inspect_json_includes_status_detail() {
        let mut record = make_record("id", "box", "dead", None);
        record.exit_code = Some(137);

        let json = inspect_json(&record).unwrap();

        // Top-level array (docker inspect) with a Docker-shaped State object.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["State"]["Status"], "dead");
        assert_eq!(parsed[0]["State"]["Running"], false);
        assert_eq!(parsed[0]["State"]["ExitCode"], 137);

        assert!(json.contains("\"status\": \"dead\""));
        assert!(json.contains("\"status_detail\""));
        assert!(json.contains("\"summary\": \"dead (Exit 137)\""));
        assert!(json.contains("a3s-box restart box"));
    }

    #[test]
    fn test_inspect_state_running_and_paused() {
        let running = make_record("id", "box", "running", Some(1));
        let parsed: serde_json::Value =
            serde_json::from_str(&inspect_json(&running).unwrap()).unwrap();
        assert_eq!(parsed[0]["State"]["Running"], true);
        assert_eq!(parsed[0]["State"]["Paused"], false);

        let paused = make_record("id", "box", "paused", Some(1));
        let parsed: serde_json::Value =
            serde_json::from_str(&inspect_json(&paused).unwrap()).unwrap();
        assert_eq!(parsed[0]["State"]["Running"], true);
        assert_eq!(parsed[0]["State"]["Paused"], true);
    }
}
