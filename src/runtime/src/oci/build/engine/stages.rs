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

    for instr in instructions {
        if let Instruction::From { alias, .. } = instr {
            if let Some(stage) = current.take() {
                stages.push(stage);
            }
            current = Some(BuildStage {
                alias: alias.clone(),
                instructions: vec![instr.clone()],
            });
        } else if let Some(ref mut stage) = current {
            stage.instructions.push(instr.clone());
        }
        // Instructions before first FROM (only ARG allowed) are attached to first stage
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
