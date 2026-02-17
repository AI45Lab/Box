//! Tests for Dockerfile parser.

#[cfg(test)]
mod tests {
    use super::super::*;

    // --- join_continuation_lines ---

    #[test]
    fn test_join_continuation_simple() {
        let input = "RUN apt-get update && \\\n    apt-get install -y curl";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("apt-get update"));
        assert!(lines[0].contains("apt-get install"));
    }

    #[test]
    fn test_join_continuation_no_continuation() {
        let input = "FROM alpine:3.19\nRUN echo hello";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_join_continuation_multiple() {
        let input = "RUN a \\\n    b \\\n    c";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('a'));
        assert!(lines[0].contains('b'));
        assert!(lines[0].contains('c'));
    }

    // --- parse_from ---

    #[test]
    fn test_parse_from_simple() {
        let result = parsers::parse_from("alpine:3.19", 1).unwrap();
        assert_eq!(
            result,
            Instruction::From {
                image: "alpine:3.19".to_string(),
                alias: None,
            }
        );
    }

    #[test]
    fn test_parse_from_with_alias() {
        let result = parsers::parse_from("golang:1.21 AS builder", 1).unwrap();
        assert_eq!(
            result,
            Instruction::From {
                image: "golang:1.21".to_string(),
                alias: Some("builder".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_from_empty() {
        assert!(parsers::parse_from("", 1).is_err());
    }

    // --- parse_run ---

    #[test]
    fn test_parse_run_shell() {
        let result = parsers::parse_run("apt-get update && apt-get install -y curl", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Run {
                command: "apt-get update && apt-get install -y curl".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_run_json() {
        let result = parsers::parse_run(r#"["echo", "hello"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Run {
                command: "echo hello".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_run_empty() {
        assert!(parsers::parse_run("", 1).is_err());
    }

    // --- parse_copy ---

    #[test]
    fn test_parse_copy_simple() {
        let result = parsers::parse_copy("app.py /workspace/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["app.py".to_string()],
                dst: "/workspace/".to_string(),
                from: None,
            }
        );
    }

    #[test]
    fn test_parse_copy_multiple_sources() {
        let result = parsers::parse_copy("file1.txt file2.txt /dest/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["file1.txt".to_string(), "file2.txt".to_string()],
                dst: "/dest/".to_string(),
                from: None,
            }
        );
    }

    #[test]
    fn test_parse_copy_from_stage() {
        let result = parsers::parse_copy("--from=builder /app/bin /usr/local/bin/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["/app/bin".to_string()],
                dst: "/usr/local/bin/".to_string(),
                from: Some("builder".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_copy_empty() {
        assert!(parsers::parse_copy("", 1).is_err());
    }

    #[test]
    fn test_parse_copy_single_arg() {
        assert!(parsers::parse_copy("onlysource", 1).is_err());
    }

    // --- parse_env ---

    #[test]
    fn test_parse_env_equals() {
        let result = parsers::parse_env("PATH=/usr/local/bin:/usr/bin", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "PATH".to_string(),
                value: "/usr/local/bin:/usr/bin".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_quoted() {
        let result = parsers::parse_env(r#"MSG="hello world""#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "MSG".to_string(),
                value: "hello world".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_legacy() {
        let result = parsers::parse_env("MY_VAR my_value", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "MY_VAR".to_string(),
                value: "my_value".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_empty() {
        assert!(parsers::parse_env("", 1).is_err());
    }

    // --- parse_entrypoint ---

    #[test]
    fn test_parse_entrypoint_exec() {
        let result = parsers::parse_entrypoint(r#"["/bin/agent", "--listen"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Entrypoint {
                exec: vec!["/bin/agent".to_string(), "--listen".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_entrypoint_shell() {
        let result = parsers::parse_entrypoint("/bin/agent --listen", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Entrypoint {
                exec: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "/bin/agent --listen".to_string(),
                ],
            }
        );
    }

    // --- parse_cmd ---

    #[test]
    fn test_parse_cmd_exec() {
        let result = parsers::parse_cmd(r#"["--port", "8080"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Cmd {
                exec: vec!["--port".to_string(), "8080".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_cmd_shell() {
        let result = parsers::parse_cmd("echo hello", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Cmd {
                exec: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hello".to_string(),
                ],
            }
        );
    }

    // --- parse_expose ---

    #[test]
    fn test_parse_expose() {
        let result = parsers::parse_expose("8080", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Expose {
                port: "8080".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_expose_with_proto() {
        let result = parsers::parse_expose("8080/tcp", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Expose {
                port: "8080/tcp".to_string(),
            }
        );
    }

    // --- parse_label ---

    #[test]
    fn test_parse_label_equals() {
        let result = parsers::parse_label("version=1.0.0", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Label {
                key: "version".to_string(),
                value: "1.0.0".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_label_quoted() {
        let result = parsers::parse_label(r#"description="My App""#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Label {
                key: "description".to_string(),
                value: "My App".to_string(),
            }
        );
    }

    // --- parse_user ---

    #[test]
    fn test_parse_user() {
        let result = parsers::parse_user("nobody", 1).unwrap();
        assert_eq!(
            result,
            Instruction::User {
                user: "nobody".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_user_with_group() {
        let result = parsers::parse_user("1000:1000", 1).unwrap();
        assert_eq!(
            result,
            Instruction::User {
                user: "1000:1000".to_string(),
            }
        );
    }

    // --- parse_arg ---

    #[test]
    fn test_parse_arg_no_default() {
        let result = parsers::parse_arg("VERSION", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Arg {
                name: "VERSION".to_string(),
                default: None,
            }
        );
    }

    #[test]
    fn test_parse_arg_with_default() {
        let result = parsers::parse_arg("VERSION=1.0.0", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Arg {
                name: "VERSION".to_string(),
                default: Some("1.0.0".to_string()),
            }
        );
    }

    // --- Full Dockerfile parsing ---

    #[test]
    fn test_parse_minimal_dockerfile() {
        let content = "FROM alpine:3.19\nCMD [\"echo\", \"hello\"]";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(
            matches!(&df.instructions[0], Instruction::From { image, .. } if image == "alpine:3.19")
        );
    }

    #[test]
    fn test_parse_complex_dockerfile() {
        let content = r#"
# Build stage
FROM python:3.12-slim

WORKDIR /app

ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

EXPOSE 8080

LABEL version="1.0.0"
LABEL maintainer="team@example.com"

USER nobody

ENTRYPOINT ["python"]
CMD ["app.py"]
"#;
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 13);
    }

    #[test]
    fn test_parse_with_continuations() {
        let content = "FROM alpine:3.19\nRUN apk add --no-cache \\\n    curl \\\n    wget";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        if let Instruction::Run { command } = &df.instructions[1] {
            assert!(command.contains("curl"));
            assert!(command.contains("wget"));
        } else {
            panic!("Expected RUN instruction");
        }
    }

    #[test]
    fn test_parse_empty_dockerfile() {
        let content = "# just a comment\n\n";
        assert!(Dockerfile::parse(content).is_err());
    }

    #[test]
    fn test_parse_no_from() {
        let content = "RUN echo hello";
        assert!(Dockerfile::parse(content).is_err());
    }

    #[test]
    fn test_parse_arg_before_from() {
        let content = "ARG VERSION=3.19\nFROM alpine:${VERSION}";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(&df.instructions[0], Instruction::Arg { .. }));
        assert!(matches!(&df.instructions[1], Instruction::From { .. }));
    }

    #[test]
    fn test_parse_comments_and_blanks() {
        let content = "\n# comment\n\nFROM alpine\n\n# another comment\nRUN echo hi\n\n";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
    }

    // --- unquote ---

    #[test]
    fn test_unquote_double() {
        assert_eq!(utils::unquote(r#""hello world""#), "hello world");
    }

    #[test]
    fn test_unquote_single() {
        assert_eq!(utils::unquote("'hello world'"), "hello world");
    }

    #[test]
    fn test_unquote_none() {
        assert_eq!(utils::unquote("hello"), "hello");
    }

    #[test]
    fn test_unquote_mismatched() {
        assert_eq!(utils::unquote(r#""hello'"#), r#""hello'"#);
    }

    // --- parse_add ---

    #[test]
    fn test_parse_add_simple() {
        let result = parsers::parse_add("app.tar.gz /app/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Add {
                src: vec!["app.tar.gz".to_string()],
                dst: "/app/".to_string(),
                chown: None,
            }
        );
    }

    #[test]
    fn test_parse_add_with_chown() {
        let result = parsers::parse_add("--chown=1000:1000 files/ /data/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Add {
                src: vec!["files/".to_string()],
                dst: "/data/".to_string(),
                chown: Some("1000:1000".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_add_url() {
        let result = parsers::parse_add("https://example.com/file.tar.gz /tmp/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Add {
                src: vec!["https://example.com/file.tar.gz".to_string()],
                dst: "/tmp/".to_string(),
                chown: None,
            }
        );
    }

    #[test]
    fn test_parse_add_empty() {
        assert!(parsers::parse_add("", 1).is_err());
    }

    #[test]
    fn test_parse_add_single_arg() {
        assert!(parsers::parse_add("onlysource", 1).is_err());
    }

    // --- parse_shell ---

    #[test]
    fn test_parse_shell_bash() {
        let result = parsers::parse_shell(r#"["/bin/bash", "-c"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Shell {
                exec: vec!["/bin/bash".to_string(), "-c".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_shell_powershell() {
        let result = parsers::parse_shell(r#"["powershell", "-command"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Shell {
                exec: vec!["powershell".to_string(), "-command".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_shell_empty() {
        assert!(parsers::parse_shell("", 1).is_err());
    }

    #[test]
    fn test_parse_shell_not_json() {
        assert!(parsers::parse_shell("/bin/bash -c", 1).is_err());
    }

    #[test]
    fn test_parse_shell_empty_array() {
        assert!(parsers::parse_shell("[]", 1).is_err());
    }

    // --- parse_stopsignal ---

    #[test]
    fn test_parse_stopsignal_name() {
        let result = parsers::parse_stopsignal("SIGTERM", 1).unwrap();
        assert_eq!(
            result,
            Instruction::StopSignal {
                signal: "SIGTERM".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_stopsignal_number() {
        let result = parsers::parse_stopsignal("9", 1).unwrap();
        assert_eq!(
            result,
            Instruction::StopSignal {
                signal: "9".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_stopsignal_empty() {
        assert!(parsers::parse_stopsignal("", 1).is_err());
    }

    // --- parse_healthcheck ---

    #[test]
    fn test_parse_healthcheck_none() {
        let result = parsers::parse_healthcheck("NONE", 1).unwrap();
        assert_eq!(
            result,
            Instruction::HealthCheck {
                cmd: None,
                interval: None,
                timeout: None,
                retries: None,
                start_period: None,
            }
        );
    }

    #[test]
    fn test_parse_healthcheck_simple_cmd() {
        let result = parsers::parse_healthcheck("CMD curl -f http://localhost/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::HealthCheck {
                cmd: Some(vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "curl -f http://localhost/".to_string(),
                ]),
                interval: None,
                timeout: None,
                retries: None,
                start_period: None,
            }
        );
    }

    #[test]
    fn test_parse_healthcheck_with_options() {
        let result = parsers::parse_healthcheck(
            "--interval=10s --timeout=5s --retries=5 --start-period=30s CMD curl -f http://localhost/",
            1,
        )
        .unwrap();
        assert_eq!(
            result,
            Instruction::HealthCheck {
                cmd: Some(vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "curl -f http://localhost/".to_string(),
                ]),
                interval: Some(10),
                timeout: Some(5),
                retries: Some(5),
                start_period: Some(30),
            }
        );
    }

    #[test]
    fn test_parse_healthcheck_json_cmd() {
        let result = parsers::parse_healthcheck(r#"CMD ["curl", "-f", "http://localhost/"]"#, 1).unwrap();
        if let Instruction::HealthCheck { cmd, .. } = &result {
            assert_eq!(
                cmd.as_ref().unwrap(),
                &vec![
                    "curl".to_string(),
                    "-f".to_string(),
                    "http://localhost/".to_string()
                ]
            );
        } else {
            panic!("Expected HealthCheck");
        }
    }

    #[test]
    fn test_parse_healthcheck_empty() {
        assert!(parsers::parse_healthcheck("", 1).is_err());
    }

    #[test]
    fn test_parse_healthcheck_no_cmd() {
        assert!(parsers::parse_healthcheck("--interval=10s", 1).is_err());
    }

    // --- parse_onbuild ---

    #[test]
    fn test_parse_onbuild_run() {
        let result = parsers::parse_onbuild("RUN echo hello", 1).unwrap();
        assert_eq!(
            result,
            Instruction::OnBuild {
                instruction: Box::new(Instruction::Run {
                    command: "echo hello".to_string(),
                }),
            }
        );
    }

    #[test]
    fn test_parse_onbuild_copy() {
        let result = parsers::parse_onbuild("COPY . /app", 1).unwrap();
        assert_eq!(
            result,
            Instruction::OnBuild {
                instruction: Box::new(Instruction::Copy {
                    src: vec![".".to_string()],
                    dst: "/app".to_string(),
                    from: None,
                }),
            }
        );
    }

    #[test]
    fn test_parse_onbuild_empty() {
        assert!(parsers::parse_onbuild("", 1).is_err());
    }

    #[test]
    fn test_parse_onbuild_onbuild() {
        assert!(parsers::parse_onbuild("ONBUILD RUN echo", 1).is_err());
    }

    #[test]
    fn test_parse_onbuild_from() {
        assert!(parsers::parse_onbuild("FROM alpine", 1).is_err());
    }

    // --- parse_volume ---

    #[test]
    fn test_parse_volume_single() {
        let result = parsers::parse_volume("/data", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Volume {
                paths: vec!["/data".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_volume_multiple() {
        let result = parsers::parse_volume("/data /var/log", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Volume {
                paths: vec!["/data".to_string(), "/var/log".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_volume_json() {
        let result = parsers::parse_volume(r#"["/data", "/var/log"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Volume {
                paths: vec!["/data".to_string(), "/var/log".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_volume_empty() {
        assert!(parsers::parse_volume("", 1).is_err());
    }

    // --- parse_duration_secs ---

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(utils::parse_duration_secs("30s", 1).unwrap(), 30);
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(utils::parse_duration_secs("5m", 1).unwrap(), 300);
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(utils::parse_duration_secs("1h", 1).unwrap(), 3600);
    }

    #[test]
    fn test_parse_duration_plain_number() {
        assert_eq!(utils::parse_duration_secs("30", 1).unwrap(), 30);
    }

    #[test]
    fn test_parse_duration_empty() {
        assert_eq!(utils::parse_duration_secs("", 1).unwrap(), 0);
    }

    // --- Dockerfile with new instructions ---

    #[test]
    fn test_parse_dockerfile_with_shell() {
        let content = "FROM alpine\nSHELL [\"/bin/bash\", \"-c\"]\nRUN echo hello";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 3);
        assert!(matches!(&df.instructions[1], Instruction::Shell { exec } if exec.len() == 2));
    }

    #[test]
    fn test_parse_dockerfile_with_healthcheck() {
        let content = "FROM alpine\nHEALTHCHECK CMD curl -f http://localhost/";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(
            &df.instructions[1],
            Instruction::HealthCheck { cmd: Some(_), .. }
        ));
    }

    #[test]
    fn test_parse_dockerfile_with_volume() {
        let content = "FROM alpine\nVOLUME /data";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(
            matches!(&df.instructions[1], Instruction::Volume { paths } if paths == &["/data".to_string()])
        );
    }

    #[test]
    fn test_parse_dockerfile_with_add() {
        let content = "FROM alpine\nADD app.tar.gz /app/";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(&df.instructions[1], Instruction::Add { .. }));
    }

    #[test]
    fn test_parse_dockerfile_with_stopsignal() {
        let content = "FROM alpine\nSTOPSIGNAL SIGKILL";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(
            matches!(&df.instructions[1], Instruction::StopSignal { signal } if signal == "SIGKILL")
        );
    }

    #[test]
    fn test_parse_dockerfile_with_onbuild() {
        let content = "FROM alpine\nONBUILD RUN echo triggered";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(&df.instructions[1], Instruction::OnBuild { .. }));
    }
}
