//! `a3s-box inspect` command — Detailed box information as JSON.

use clap::Args;
use serde::Serialize;

use crate::resolve;
use crate::state::{BoxRecord, StateFile};
use crate::status;

#[derive(Args)]
pub struct InspectArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    let json = inspect_json(record)?;
    println!("{json}");

    Ok(())
}

#[derive(Serialize)]
struct InspectView<'a> {
    #[serde(flatten)]
    record: &'a BoxRecord,
    status_detail: status::StatusDetails,
}

fn inspect_json(record: &BoxRecord) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&InspectView {
        record,
        status_detail: status::status_details(record),
    })
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

        assert!(json.contains("\"status\": \"dead\""));
        assert!(json.contains("\"status_detail\""));
        assert!(json.contains("\"summary\": \"dead (Exit 137)\""));
        assert!(json.contains("a3s-box restart box"));
    }
}
