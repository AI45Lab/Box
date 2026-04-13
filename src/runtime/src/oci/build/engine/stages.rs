//! Multi-stage build support.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerfile::Instruction;

/// A build stage: a FROM instruction followed by its body instructions.
pub(super) struct BuildStage {
    pub(super) alias: Option<String>,
    pub(super) instructions: Vec<Instruction>,
}

/// Split a flat list of instructions into stages, each starting with FROM.
pub(super) fn split_into_stages(instructions: &[Instruction]) -> Vec<BuildStage> {
    let mut stages = Vec::new();
    let mut current: Option<BuildStage> = None;
    // Buffer for instructions before first FROM (only ARG allowed per OCI spec)
    let mut pre_from_instructions: Vec<Instruction> = Vec::new();

    for instr in instructions {
        if let Instruction::From { alias, .. } = instr {
            if let Some(stage) = current.take() {
                stages.push(stage);
            }
            // Pre-FROM instructions (ARG only) are attached to first stage
            let mut first_instructions = pre_from_instructions.clone();
            first_instructions.push(instr.clone());
            current = Some(BuildStage {
                alias: alias.clone(),
                instructions: first_instructions,
            });
            pre_from_instructions.clear();
        } else if let Some(ref mut stage) = current {
            stage.instructions.push(instr.clone());
        } else {
            // Before first FROM: only ARG instructions are allowed
            pre_from_instructions.push(instr.clone());
        }
    }

    if let Some(stage) = current {
        stages.push(stage);
    }

    stages
}

/// Resolve a stage reference (name or index) to its rootfs path.
pub(super) fn resolve_stage_rootfs<'a>(
    from_ref: &str,
    completed_stages: &'a [(Option<String>, PathBuf)],
) -> Result<&'a Path> {
    // Try by alias first
    for (alias, rootfs) in completed_stages {
        if let Some(a) = alias {
            if a == from_ref {
                return Ok(rootfs);
            }
        }
    }

    // Try by index
    if let Ok(idx) = from_ref.parse::<usize>() {
        if idx < completed_stages.len() {
            return Ok(&completed_stages[idx].1);
        }
    }

    Err(BoxError::BuildError(format!(
        "COPY --from={}: stage not found (available: {})",
        from_ref,
        completed_stages
            .iter()
            .enumerate()
            .map(|(i, (alias, _))| {
                if let Some(a) = alias {
                    format!("{} ({})", i, a)
                } else {
                    i.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_from(image: &str, alias: Option<&str>) -> Instruction {
        Instruction::From {
            image: image.to_string(),
            alias: alias.map(|s| s.to_string()),
        }
    }

    fn make_run(cmd: &str) -> Instruction {
        Instruction::Run {
            command: cmd.to_string(),
        }
    }

    fn make_copy(src: &str, dst: &str) -> Instruction {
        Instruction::Copy {
            src: vec![src.to_string()],
            dst: dst.to_string(),
            from: None,
        }
    }

    // --- split_into_stages tests ---

    #[test]
    fn test_split_into_stages_single_stage() {
        let instructions = vec![make_from("alpine:3.19", None), make_run("echo hello")];
        let stages = split_into_stages(&instructions);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].alias, None);
        assert_eq!(stages[0].instructions.len(), 2);
    }

    #[test]
    fn test_split_into_stages_single_named() {
        let instructions = vec![
            make_from("golang:1.21", Some("builder")),
            make_run("go build"),
        ];
        let stages = split_into_stages(&instructions);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].alias, Some("builder".to_string()));
    }

    #[test]
    fn test_split_into_stages_multiple_stages() {
        let instructions = vec![
            make_from("golang:1.21", Some("builder")),
            make_run("go build -o app"),
            make_copy("app", "/usr/local/bin/"),
            make_from("alpine:3.19", None),
            make_run("apk add --no-cache"),
        ];
        let stages = split_into_stages(&instructions);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].alias, Some("builder".to_string()));
        assert_eq!(stages[0].instructions.len(), 3);
        assert_eq!(stages[1].alias, None);
        assert_eq!(stages[1].instructions.len(), 2);
    }

    #[test]
    fn test_split_into_stages_three_stages() {
        let instructions = vec![
            make_from("node:20", Some("deps")),
            make_run("npm ci"),
            make_from("node:20-alpine", Some("builder")),
            make_copy("package*.json", "/app/"),
            make_run("npm run build"),
            make_from("node:20-alpine", None),
            make_copy("--from=builder /app/dist", "/app/"),
        ];
        let stages = split_into_stages(&instructions);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].alias, Some("deps".to_string()));
        assert_eq!(stages[1].alias, Some("builder".to_string()));
        assert_eq!(stages[2].alias, None);
    }

    #[test]
    fn test_split_into_stages_instructions_before_from_attached() {
        // ARG instructions before first FROM should be attached to first stage
        let arg = Instruction::Arg {
            name: "VERSION".to_string(),
            default: Some("1.0.0".to_string()),
        };
        let instructions = vec![arg, make_from("alpine:3.19", None), make_run("echo hello")];
        let stages = split_into_stages(&instructions);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].instructions.len(), 3);
    }

    #[test]
    fn test_split_into_stages_empty() {
        let stages = split_into_stages(&[]);
        assert!(stages.is_empty());
    }

    // --- resolve_stage_rootfs tests ---

    #[test]
    fn test_resolve_stage_rootfs_by_alias() {
        let stages = vec![
            (Some("builder".to_string()), PathBuf::from("/tmp/stage0")),
            (Some("final".to_string()), PathBuf::from("/tmp/stage1")),
        ];
        let result = resolve_stage_rootfs("builder", &stages).unwrap();
        assert_eq!(result, Path::new("/tmp/stage0"));
    }

    #[test]
    fn test_resolve_stage_rootfs_by_alias_unnamed() {
        let stages = vec![
            (None, PathBuf::from("/tmp/stage0")),
            (None, PathBuf::from("/tmp/stage1")),
        ];
        // By index when no aliases
        let result = resolve_stage_rootfs("1", &stages).unwrap();
        assert_eq!(result, Path::new("/tmp/stage1"));
    }

    #[test]
    fn test_resolve_stage_rootfs_by_index() {
        let stages = vec![
            (Some("builder".to_string()), PathBuf::from("/tmp/stage0")),
            (None, PathBuf::from("/tmp/stage1")),
        ];
        let result = resolve_stage_rootfs("0", &stages).unwrap();
        assert_eq!(result, Path::new("/tmp/stage0"));
        let result = resolve_stage_rootfs("1", &stages).unwrap();
        assert_eq!(result, Path::new("/tmp/stage1"));
    }

    #[test]
    fn test_resolve_stage_rootfs_out_of_bounds() {
        let stages = vec![(Some("builder".to_string()), PathBuf::from("/tmp/stage0"))];
        let result = resolve_stage_rootfs("5", &stages);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stage not found"));
    }

    #[test]
    fn test_resolve_stage_rootfs_invalid_reference() {
        let stages = vec![(Some("builder".to_string()), PathBuf::from("/tmp/stage0"))];
        let result = resolve_stage_rootfs("nonexistent", &stages);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stage not found"));
    }

    #[test]
    fn test_resolve_stage_rootfs_empty_stages() {
        let stages: Vec<(Option<String>, PathBuf)> = vec![];
        let result = resolve_stage_rootfs("0", &stages);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_stage_rootfs_error_lists_available() {
        let stages = vec![
            (Some("builder".to_string()), PathBuf::from("/tmp/stage0")),
            (None, PathBuf::from("/tmp/stage1")),
        ];
        let result = resolve_stage_rootfs("nonexistent", &stages);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("stage not found"));
        assert!(err.contains("builder"));
        // Unnamed stage 1 is listed as just "1"
        assert!(err.contains("1"));
    }
}
