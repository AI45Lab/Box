//! Individual instruction parsers for Dockerfile instructions.

use a3s_box_core::error::{BoxError, Result};

use super::utils::{parse_duration_secs, parse_json_array, shell_split, unquote};
use super::{split_first_word, Instruction};

pub(super) fn parse_from(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: FROM requires an image argument",
            line_num
        )));
    }

    // Check for AS alias: FROM image AS alias
    let parts: Vec<&str> = rest.splitn(3, char::is_whitespace).collect();
    let (image, alias) = if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("AS") {
        (parts[0].to_string(), Some(parts[2].trim().to_string()))
    } else {
        (parts[0].to_string(), None)
    };

    Ok(Instruction::From { image, alias })
}

pub(super) fn parse_run(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: RUN requires a command",
            line_num
        )));
    }

    // If JSON array form, extract and join
    let command = if rest.starts_with('[') {
        let parts = parse_json_array(rest, line_num)?;
        parts.join(" ")
    } else {
        rest.to_string()
    };

    Ok(Instruction::Run { command })
}

pub(super) fn parse_copy(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: COPY requires source and destination",
            line_num
        )));
    }

    // Check for --from=<stage> flag
    let (from, remaining) = if rest.starts_with("--from=") {
        let (flag, after) = split_first_word(rest);
        let stage = flag.strip_prefix("--from=").unwrap_or("").to_string();
        (Some(stage), after)
    } else {
        (None, rest)
    };

    // Split remaining into src... dst (last element is dst)
    let parts: Vec<&str> = shell_split(remaining);
    if parts.len() < 2 {
        return Err(BoxError::BuildError(format!(
            "Line {}: COPY requires at least one source and a destination",
            line_num
        )));
    }

    // parts.len() >= 2 guaranteed by the check above
    let dst = parts[parts.len() - 1].to_string();
    let src: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();

    Ok(Instruction::Copy { src, dst, from })
}

pub(super) fn parse_workdir(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: WORKDIR requires a path",
            line_num
        )));
    }
    Ok(Instruction::Workdir {
        path: rest.to_string(),
    })
}

pub(super) fn parse_env(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ENV requires a key and value",
            line_num
        )));
    }

    // Two forms:
    // ENV KEY=VALUE  (or KEY="VALUE")
    // ENV KEY VALUE
    if let Some(eq_pos) = rest.find('=') {
        // Check it's not inside a value after a space
        let space_pos = rest.find(char::is_whitespace);
        if space_pos.is_none_or(|sp| eq_pos < sp) {
            let key = rest[..eq_pos].to_string();
            let value = unquote(&rest[eq_pos + 1..]);
            return Ok(Instruction::Env { key, value });
        }
    }

    // Legacy form: ENV KEY VALUE
    let (key, value) = split_first_word(rest);
    Ok(Instruction::Env {
        key: key.to_string(),
        value: value.to_string(),
    })
}

pub(super) fn parse_entrypoint(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ENTRYPOINT requires an argument",
            line_num
        )));
    }

    let exec = if rest.starts_with('[') {
        parse_json_array(rest, line_num)?
    } else {
        // Shell form: wrap in sh -c
        vec!["/bin/sh".to_string(), "-c".to_string(), rest.to_string()]
    };

    Ok(Instruction::Entrypoint { exec })
}

pub(super) fn parse_cmd(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: CMD requires an argument",
            line_num
        )));
    }

    let exec = if rest.starts_with('[') {
        parse_json_array(rest, line_num)?
    } else {
        // Shell form: wrap in sh -c
        vec!["/bin/sh".to_string(), "-c".to_string(), rest.to_string()]
    };

    Ok(Instruction::Cmd { exec })
}

pub(super) fn parse_expose(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: EXPOSE requires a port",
            line_num
        )));
    }
    Ok(Instruction::Expose {
        port: rest.split_whitespace().next().unwrap_or(rest).to_string(),
    })
}

pub(super) fn parse_label(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: LABEL requires key=value",
            line_num
        )));
    }

    // LABEL key=value
    if let Some(eq_pos) = rest.find('=') {
        let key = rest[..eq_pos].trim().to_string();
        let value = unquote(rest[eq_pos + 1..].trim());
        Ok(Instruction::Label { key, value })
    } else {
        // LABEL key value (legacy)
        let (key, value) = split_first_word(rest);
        Ok(Instruction::Label {
            key: key.to_string(),
            value: unquote(value),
        })
    }
}

pub(super) fn parse_user(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: USER requires a username",
            line_num
        )));
    }
    Ok(Instruction::User {
        user: rest.split_whitespace().next().unwrap_or(rest).to_string(),
    })
}

pub(super) fn parse_arg(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ARG requires a name",
            line_num
        )));
    }

    if let Some(eq_pos) = rest.find('=') {
        let name = rest[..eq_pos].to_string();
        let default = Some(unquote(&rest[eq_pos + 1..]));
        Ok(Instruction::Arg { name, default })
    } else {
        Ok(Instruction::Arg {
            name: rest.trim().to_string(),
            default: None,
        })
    }
}

pub(super) fn parse_add(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ADD requires source and destination",
            line_num
        )));
    }

    // Check for --chown=<user> flag
    let (chown, remaining) = if rest.starts_with("--chown=") {
        let (flag, after) = split_first_word(rest);
        let user = flag.strip_prefix("--chown=").unwrap_or("").to_string();
        (Some(user), after)
    } else {
        (None, rest)
    };

    // Split remaining into src... dst (last element is dst)
    let parts: Vec<&str> = shell_split(remaining);
    if parts.len() < 2 {
        return Err(BoxError::BuildError(format!(
            "Line {}: ADD requires at least one source and a destination",
            line_num
        )));
    }

    // parts.len() >= 2 guaranteed by the check above
    let dst = parts[parts.len() - 1].to_string();
    let src: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();

    Ok(Instruction::Add { src, dst, chown })
}

pub(super) fn parse_shell(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: SHELL requires a JSON array argument",
            line_num
        )));
    }

    if !rest.starts_with('[') {
        return Err(BoxError::BuildError(format!(
            "Line {}: SHELL must use JSON array form (e.g., SHELL [\"/bin/bash\", \"-c\"])",
            line_num
        )));
    }

    let exec = parse_json_array(rest, line_num)?;
    if exec.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: SHELL requires at least one element",
            line_num
        )));
    }

    Ok(Instruction::Shell { exec })
}

pub(super) fn parse_stopsignal(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: STOPSIGNAL requires a signal",
            line_num
        )));
    }

    Ok(Instruction::StopSignal {
        signal: rest.trim().to_string(),
    })
}

pub(super) fn parse_healthcheck(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: HEALTHCHECK requires CMD or NONE",
            line_num
        )));
    }

    // HEALTHCHECK NONE
    if rest.trim().eq_ignore_ascii_case("NONE") {
        return Ok(Instruction::HealthCheck {
            cmd: None,
            interval: None,
            timeout: None,
            retries: None,
            start_period: None,
        });
    }

    // Parse options and CMD
    let mut interval = None;
    let mut timeout = None;
    let mut retries = None;
    let mut start_period = None;
    let mut remaining = rest;

    loop {
        let trimmed = remaining.trim_start();
        if trimmed.starts_with("--interval=") {
            let (flag, after) = split_first_word(trimmed);
            interval = Some(parse_duration_secs(
                flag.strip_prefix("--interval=").unwrap_or("30s"),
                line_num,
            )?);
            remaining = after;
        } else if trimmed.starts_with("--timeout=") {
            let (flag, after) = split_first_word(trimmed);
            timeout = Some(parse_duration_secs(
                flag.strip_prefix("--timeout=").unwrap_or("30s"),
                line_num,
            )?);
            remaining = after;
        } else if trimmed.starts_with("--retries=") {
            let (flag, after) = split_first_word(trimmed);
            let val = flag.strip_prefix("--retries=").unwrap_or("3");
            retries = Some(val.parse::<u32>().map_err(|_| {
                BoxError::BuildError(format!(
                    "Line {}: Invalid --retries value: {}",
                    line_num, val
                ))
            })?);
            remaining = after;
        } else if trimmed.starts_with("--start-period=") {
            let (flag, after) = split_first_word(trimmed);
            start_period = Some(parse_duration_secs(
                flag.strip_prefix("--start-period=").unwrap_or("0s"),
                line_num,
            )?);
            remaining = after;
        } else {
            break;
        }
    }

    // Expect CMD keyword
    let trimmed = remaining.trim_start();
    let (keyword, cmd_rest) = split_first_word(trimmed);
    if !keyword.eq_ignore_ascii_case("CMD") {
        return Err(BoxError::BuildError(format!(
            "Line {}: HEALTHCHECK expected CMD, got '{}'",
            line_num, keyword
        )));
    }

    if cmd_rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: HEALTHCHECK CMD requires a command",
            line_num
        )));
    }

    let cmd = if cmd_rest.starts_with('[') {
        parse_json_array(cmd_rest, line_num)?
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            cmd_rest.to_string(),
        ]
    };

    Ok(Instruction::HealthCheck {
        cmd: Some(cmd),
        interval,
        timeout,
        retries,
        start_period,
    })
}

pub(super) fn parse_onbuild(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ONBUILD requires an instruction",
            line_num
        )));
    }

    // Parse the inner instruction recursively
    let inner = super::parse_instruction(rest, line_num)?;

    // ONBUILD ONBUILD is not allowed
    if matches!(inner, Instruction::OnBuild { .. }) {
        return Err(BoxError::BuildError(format!(
            "Line {}: ONBUILD ONBUILD is not allowed",
            line_num
        )));
    }

    // ONBUILD FROM is not allowed
    if matches!(inner, Instruction::From { .. }) {
        return Err(BoxError::BuildError(format!(
            "Line {}: ONBUILD FROM is not allowed",
            line_num
        )));
    }

    Ok(Instruction::OnBuild {
        instruction: Box::new(inner),
    })
}

pub(super) fn parse_volume(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: VOLUME requires at least one path",
            line_num
        )));
    }

    let paths = if rest.starts_with('[') {
        parse_json_array(rest, line_num)?
    } else {
        rest.split_whitespace().map(|s| s.to_string()).collect()
    };

    if paths.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: VOLUME requires at least one path",
            line_num
        )));
    }

    Ok(Instruction::Volume { paths })
}
