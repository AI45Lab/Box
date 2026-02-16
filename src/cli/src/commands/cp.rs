//! `a3s-box cp` command — Copy files or directories between host and a running box.
//!
//! Uses the exec channel to transfer content via base64 encoding.
//! Single files are transferred as raw base64. Directories are archived
//! with `tar` before transfer.
//!
//! Syntax:
//!   a3s-box cp <box>:/path/in/box /host/path   (box → host)
//!   a3s-box cp /host/path <box>:/path/in/box   (host → box)

use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
use a3s_box_runtime::ExecClient;
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

/// Timeout for directory transfers (60 seconds).
const DIR_TRANSFER_TIMEOUT_NS: u64 = 60_000_000_000;

#[derive(Args)]
pub struct CpArgs {
    /// Source path (HOST_PATH or BOX:CONTAINER_PATH)
    pub src: String,

    /// Destination path (HOST_PATH or BOX:CONTAINER_PATH)
    pub dst: String,
}

/// Parsed copy endpoint — either a host path or a box:path pair.
enum Endpoint {
    Host(String),
    Box { name: String, path: String },
}

fn parse_endpoint(s: &str) -> Endpoint {
    // Docker convention: "container:/path" means container path
    // A bare path (no colon, or colon after drive letter on Windows) means host
    if let Some((name, path)) = s.split_once(':') {
        // Avoid treating "C:\path" as a container reference
        if name.len() > 1 {
            return Endpoint::Box {
                name: name.to_string(),
                path: path.to_string(),
            };
        }
    }
    Endpoint::Host(s.to_string())
}

pub async fn execute(args: CpArgs) -> Result<(), Box<dyn std::error::Error>> {
    let src = parse_endpoint(&args.src);
    let dst = parse_endpoint(&args.dst);

    match (src, dst) {
        (Endpoint::Box { name, path }, Endpoint::Host(host_path)) => {
            copy_from_box(&name, &path, &host_path).await
        }
        (Endpoint::Host(host_path), Endpoint::Box { name, path }) => {
            copy_to_box(&host_path, &name, &path).await
        }
        (Endpoint::Host(_), Endpoint::Host(_)) => Err(
            "Both source and destination are host paths. One must be a box path (BOX:/path)."
                .into(),
        ),
        (Endpoint::Box { .. }, Endpoint::Box { .. }) => {
            Err("Copying between two boxes is not supported. Copy to host first.".into())
        }
    }
}

/// Copy a file or directory from a box to the host.
async fn copy_from_box(
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = connect_exec(box_name).await?;

    if is_directory_in_box(&client, box_path).await? {
        copy_dir_from_box(&client, box_name, box_path, host_path).await
    } else {
        copy_file_from_box(&client, box_name, box_path, host_path).await
    }
}

/// Copy a file or directory from the host to a box.
async fn copy_to_box(
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta =
        std::fs::metadata(host_path).map_err(|e| format!("Failed to stat {host_path}: {e}"))?;

    let client = connect_exec(box_name).await?;

    if meta.is_dir() {
        copy_dir_to_box(&client, host_path, box_name, box_path).await
    } else {
        copy_file_to_box(&client, host_path, box_name, box_path).await
    }
}

/// Check if a path is a directory inside the box.
async fn is_directory_in_box(
    client: &ExecClient,
    box_path: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let request = ExecRequest {
        cmd: vec!["test".to_string(), "-d".to_string(), box_path.to_string()],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;
    Ok(output.exit_code == 0)
}

// ---------------------------------------------------------------------------
// Single-file transfers
// ---------------------------------------------------------------------------

/// Copy a single file from a box to the host.
async fn copy_file_from_box(
    client: &ExecClient,
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("base64 < {}", shell_escape(box_path)),
        ],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to read {box_path} in box: {stderr}").into());
    }

    use base64::Engine;
    let encoded = String::from_utf8_lossy(&output.stdout);
    let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .map_err(|e| format!("Failed to decode file content: {e}"))?;

    std::fs::write(host_path, &decoded)
        .map_err(|e| format!("Failed to write to {host_path}: {e}"))?;

    println!(
        "{box_name}:{box_path} → {host_path} ({} bytes)",
        decoded.len()
    );
    Ok(())
}

/// Copy a single file from the host to a box.
async fn copy_file_to_box(
    client: &ExecClient,
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let content =
        std::fs::read(host_path).map_err(|e| format!("Failed to read {host_path}: {e}"))?;

    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&content);

    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "echo '{}' | base64 -d > {}",
                encoded,
                shell_escape(box_path)
            ),
        ],
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to write {box_path} in box: {stderr}").into());
    }

    println!(
        "{host_path} → {box_name}:{box_path} ({} bytes)",
        content.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory transfers
// ---------------------------------------------------------------------------

/// Copy a directory from a box to the host using tar.
async fn copy_dir_from_box(
    client: &ExecClient,
    box_name: &str,
    box_path: &str,
    host_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Archive the directory inside the box and base64-encode it
    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("tar -cf - -C {} . | base64", shell_escape(box_path)),
        ],
        timeout_ns: DIR_TRANSFER_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to archive {box_path} in box: {stderr}").into());
    }

    // Decode base64 tar archive
    use base64::Engine;
    let encoded = String::from_utf8_lossy(&output.stdout);
    let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let tar_data = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .map_err(|e| format!("Failed to decode tar archive: {e}"))?;

    // Create destination directory and extract
    std::fs::create_dir_all(host_path)
        .map_err(|e| format!("Failed to create directory {host_path}: {e}"))?;

    extract_tar_to_dir(&tar_data, host_path)?;

    println!(
        "{box_name}:{box_path}/ → {host_path}/ ({} bytes archived)",
        tar_data.len()
    );
    Ok(())
}

/// Copy a directory from the host to a box using tar.
async fn copy_dir_to_box(
    client: &ExecClient,
    host_path: &str,
    box_name: &str,
    box_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create tar archive of the host directory
    let tar_data = create_tar_from_dir(host_path)?;

    // Base64-encode and send to box
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&tar_data);

    // Create destination directory and extract inside the box
    let request = ExecRequest {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "mkdir -p {} && echo '{}' | base64 -d | tar -xf - -C {}",
                shell_escape(box_path),
                encoded,
                shell_escape(box_path)
            ),
        ],
        timeout_ns: DIR_TRANSFER_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

    if output.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to extract archive in box at {box_path}: {stderr}").into());
    }

    println!(
        "{host_path}/ → {box_name}:{box_path}/ ({} bytes archived)",
        tar_data.len()
    );
    Ok(())
}

/// Create a tar archive from a host directory using the `tar` command.
fn create_tar_from_dir(dir_path: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("tar")
        .args(["-cf", "-", "-C", dir_path, "."])
        .output()
        .map_err(|e| format!("Failed to run tar: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tar failed: {stderr}").into());
    }

    Ok(output.stdout)
}

/// Extract a tar archive to a host directory using the `tar` command.
fn extract_tar_to_dir(tar_data: &[u8], dir_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new("tar")
        .args(["-xf", "-", "-C", dir_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run tar: {e}"))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(tar_data)
            .map_err(|e| format!("Failed to write tar data: {e}"))?;
    }
    // Close stdin by dropping it
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for tar: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tar extraction failed: {stderr}").into());
    }

    Ok(())
}

/// Connect to a box's exec server.
async fn connect_exec(box_name: &str) -> Result<ExecClient, Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, box_name)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
        record.exec_socket_path.clone()
    } else {
        record.box_dir.join("sockets").join("exec.sock")
    };

    if !exec_socket_path.exists() {
        return Err(format!(
            "Exec socket not found for box {} at {}",
            record.name,
            exec_socket_path.display()
        )
        .into());
    }

    ExecClient::connect(&exec_socket_path)
        .await
        .map_err(|e| e.into())
}

/// Minimal shell escaping for a file path.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Endpoint parsing tests ---

    #[test]
    fn test_parse_endpoint_host_path() {
        match parse_endpoint("/tmp/file.txt") {
            Endpoint::Host(p) => assert_eq!(p, "/tmp/file.txt"),
            _ => panic!("Expected Host endpoint"),
        }
    }

    #[test]
    fn test_parse_endpoint_box_path() {
        match parse_endpoint("mybox:/tmp/file.txt") {
            Endpoint::Box { name, path } => {
                assert_eq!(name, "mybox");
                assert_eq!(path, "/tmp/file.txt");
            }
            _ => panic!("Expected Box endpoint"),
        }
    }

    #[test]
    fn test_parse_endpoint_single_char_name_is_host() {
        // Single-char prefix treated as drive letter (host path)
        match parse_endpoint("C:/path") {
            Endpoint::Host(p) => assert_eq!(p, "C:/path"),
            _ => panic!("Expected Host endpoint for drive letter"),
        }
    }

    #[test]
    fn test_parse_endpoint_relative_host_path() {
        match parse_endpoint("./local/file") {
            Endpoint::Host(p) => assert_eq!(p, "./local/file"),
            _ => panic!("Expected Host endpoint"),
        }
    }

    // --- Shell escape tests ---

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("/tmp/file"), "'/tmp/file'");
    }

    #[test]
    fn test_shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(
            shell_escape("/path/with spaces/file"),
            "'/path/with spaces/file'"
        );
    }

    // --- Tar helper tests ---

    #[test]
    fn test_create_tar_from_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();

        // Create some test files
        std::fs::write(dir.join("file1.txt"), "hello").unwrap();
        std::fs::write(dir.join("file2.txt"), "world").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("subdir").join("nested.txt"), "nested").unwrap();

        let tar_data = create_tar_from_dir(dir.to_str().unwrap()).unwrap();
        assert!(!tar_data.is_empty());
    }

    #[test]
    fn test_create_and_extract_tar_roundtrip() {
        let src_dir = tempfile::TempDir::new().unwrap();
        let dst_dir = tempfile::TempDir::new().unwrap();

        // Create test content
        std::fs::write(src_dir.path().join("hello.txt"), "hello world").unwrap();
        std::fs::create_dir(src_dir.path().join("sub")).unwrap();
        std::fs::write(
            src_dir.path().join("sub").join("nested.txt"),
            "nested content",
        )
        .unwrap();

        // Tar and extract
        let tar_data = create_tar_from_dir(src_dir.path().to_str().unwrap()).unwrap();
        extract_tar_to_dir(&tar_data, dst_dir.path().to_str().unwrap()).unwrap();

        // Verify content
        let hello = std::fs::read_to_string(dst_dir.path().join("hello.txt")).unwrap();
        assert_eq!(hello, "hello world");

        let nested =
            std::fs::read_to_string(dst_dir.path().join("sub").join("nested.txt")).unwrap();
        assert_eq!(nested, "nested content");
    }

    #[test]
    fn test_create_tar_nonexistent_dir() {
        let result = create_tar_from_dir("/nonexistent/path/a3s_test_12345");
        assert!(result.is_err());
    }

    // --- Constant tests ---

    #[test]
    fn test_dir_transfer_timeout() {
        assert_eq!(DIR_TRANSFER_TIMEOUT_NS, 60_000_000_000);
    }
}
