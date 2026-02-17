//! Dockerfile parser.
//!
//! Parses a Dockerfile into a sequence of build instructions.
//! Supports line continuations (`\`), comments, and both shell and JSON
//! (exec) forms for CMD/ENTRYPOINT.

use a3s_box_core::error::{BoxError, Result};

mod parsers;
mod tests;
mod utils;

/// A single Dockerfile instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// `FROM <image> [AS <alias>]`
    From {
        image: String,
        alias: Option<String>,
    },
    /// `RUN <command>` (shell form)
    Run { command: String },
    /// `COPY [--from=<stage>] <src>... <dst>`
    Copy {
        src: Vec<String>,
        dst: String,
        from: Option<String>,
    },
    /// `WORKDIR <path>`
    Workdir { path: String },
    /// `ENV <key>=<value>` or `ENV <key> <value>`
    Env { key: String, value: String },
    /// `ENTRYPOINT ["exec", "form"]` or `ENTRYPOINT command`
    Entrypoint { exec: Vec<String> },
    /// `CMD ["exec", "form"]` or `CMD command`
    Cmd { exec: Vec<String> },
    /// `EXPOSE <port>[/<proto>]`
    Expose { port: String },
    /// `LABEL <key>=<value> ...`
    Label { key: String, value: String },
    /// `USER <user>[:<group>]`
    User { user: String },
    /// `ARG <name>[=<default>]`
    Arg {
        name: String,
        default: Option<String>,
    },
    /// `ADD [--chown=<user>] <src>... <dst>`
    Add {
        src: Vec<String>,
        dst: String,
        chown: Option<String>,
    },
    /// `SHELL ["executable", "param1", ...]`
    Shell { exec: Vec<String> },
    /// `STOPSIGNAL <signal>`
    StopSignal { signal: String },
    /// `HEALTHCHECK [OPTIONS] CMD command` or `HEALTHCHECK NONE`
    HealthCheck {
        cmd: Option<Vec<String>>,
        interval: Option<u64>,
        timeout: Option<u64>,
        retries: Option<u32>,
        start_period: Option<u64>,
    },
    /// `ONBUILD <instruction>`
    OnBuild { instruction: Box<Instruction> },
    /// `VOLUME <path>...`
    Volume { paths: Vec<String> },
}

/// Parsed Dockerfile: a list of instructions in order.
#[derive(Debug, Clone)]
pub struct Dockerfile {
    pub instructions: Vec<Instruction>,
}

impl Dockerfile {
    /// Parse a Dockerfile from its text content.
    pub fn parse(content: &str) -> Result<Self> {
        let logical_lines = join_continuation_lines(content);
        let mut instructions = Vec::new();

        for (line_num, line) in logical_lines.iter().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let instruction = parse_instruction(trimmed, line_num + 1)?;
            instructions.push(instruction);
        }

        if instructions.is_empty() {
            return Err(BoxError::BuildError(
                "Dockerfile is empty or contains no instructions".to_string(),
            ));
        }

        // Validate: first non-ARG instruction must be FROM
        let first_non_arg = instructions
            .iter()
            .find(|i| !matches!(i, Instruction::Arg { .. }));
        if !matches!(first_non_arg, Some(Instruction::From { .. })) {
            return Err(BoxError::BuildError(
                "First instruction must be FROM (or ARG before FROM)".to_string(),
            ));
        }

        Ok(Dockerfile { instructions })
    }

    /// Parse a Dockerfile from a file path.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to read Dockerfile at {}: {}",
                path.display(),
                e
            ))
        })?;
        Self::parse(&content)
    }
}

/// Join lines ending with `\` into single logical lines.
fn join_continuation_lines(content: &str) -> Vec<String> {
    let mut logical_lines = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        if let Some(stripped) = line.strip_suffix('\\') {
            // Remove trailing backslash and append
            current.push_str(stripped.trim_end());
            current.push(' ');
        } else {
            current.push_str(line);
            logical_lines.push(current.clone());
            current.clear();
        }
    }

    // Handle trailing continuation without final line
    if !current.is_empty() {
        logical_lines.push(current);
    }

    logical_lines
}

/// Parse a single logical line into an Instruction.
pub(super) fn parse_instruction(line: &str, line_num: usize) -> Result<Instruction> {
    // Split into keyword and rest
    let (keyword, rest) = split_first_word(line);
    let keyword_upper = keyword.to_uppercase();

    match keyword_upper.as_str() {
        "FROM" => parsers::parse_from(rest, line_num),
        "RUN" => parsers::parse_run(rest, line_num),
        "COPY" => parsers::parse_copy(rest, line_num),
        "WORKDIR" => parsers::parse_workdir(rest, line_num),
        "ENV" => parsers::parse_env(rest, line_num),
        "ENTRYPOINT" => parsers::parse_entrypoint(rest, line_num),
        "CMD" => parsers::parse_cmd(rest, line_num),
        "EXPOSE" => parsers::parse_expose(rest, line_num),
        "LABEL" => parsers::parse_label(rest, line_num),
        "USER" => parsers::parse_user(rest, line_num),
        "ARG" => parsers::parse_arg(rest, line_num),
        "ADD" => parsers::parse_add(rest, line_num),
        "SHELL" => parsers::parse_shell(rest, line_num),
        "STOPSIGNAL" => parsers::parse_stopsignal(rest, line_num),
        "HEALTHCHECK" => parsers::parse_healthcheck(rest, line_num),
        "ONBUILD" => parsers::parse_onbuild(rest, line_num),
        "VOLUME" => parsers::parse_volume(rest, line_num),
        // Silently ignore unsupported instructions with a warning
        "MAINTAINER" => {
            tracing::warn!(
                line = line_num,
                instruction = keyword_upper.as_str(),
                "Deprecated Dockerfile instruction, skipping"
            );
            Ok(Instruction::Label {
                key: "a3s.build.skipped.maintainer".to_string(),
                value: rest.to_string(),
            })
        }
        _ => Err(BoxError::BuildError(format!(
            "Line {}: Unknown instruction '{}'",
            line_num, keyword
        ))),
    }
}

/// Split a string into the first word and the rest.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(pos) => (&s[..pos], s[pos..].trim_start()),
        None => (s, ""),
    }
}

/// Parse a single instruction line (used by ONBUILD trigger execution).
pub fn parse_single_instruction(line: &str) -> Result<Instruction> {
    parse_instruction(line.trim(), 0)
}
