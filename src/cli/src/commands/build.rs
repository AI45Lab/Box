//! `a3s-box build` command — Build an image from a Dockerfile or Containerfile.
//!
//! Parses a Dockerfile/Containerfile, pulls the base image, executes instructions,
//! and produces an OCI image stored in the local image store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct BuildArgs {
    /// Build context directory (contains Dockerfile/Containerfile and source files)
    #[arg(default_value = ".")]
    pub path: String,

    /// Name and optionally tag for the image (e.g., "myimage:latest")
    #[arg(short = 't', long = "tag")]
    pub tag: Option<String>,

    /// Path to Dockerfile/Containerfile (default: <PATH>/Dockerfile, then <PATH>/Containerfile)
    #[arg(short = 'f', long = "file")]
    pub file: Option<String>,

    /// Set build-time variables (KEY=VALUE), can be repeated
    #[arg(long = "build-arg")]
    pub build_arg: Vec<String>,

    /// Suppress build output
    #[arg(short, long)]
    pub quiet: bool,

    /// Target platform for the build (e.g., "linux/amd64").
    ///
    /// Multi-platform image indexes are not supported yet.
    #[arg(long)]
    pub platform: Option<String>,

    /// Build only up to the named (or indexed) stage in a multi-stage build.
    #[arg(long)]
    pub target: Option<String>,

    /// Do not use the layer build cache; rebuild every layer.
    #[arg(long = "no-cache")]
    pub no_cache: bool,
}

pub async fn execute(args: BuildArgs) -> Result<(), Box<dyn std::error::Error>> {
    let context_dir = PathBuf::from(&args.path)
        .canonicalize()
        .map_err(|e| format!("Invalid build context path '{}': {}", args.path, e))?;

    if !context_dir.is_dir() {
        return Err(format!(
            "Build context '{}' is not a directory",
            context_dir.display()
        )
        .into());
    }

    let dockerfile_path = resolve_build_file(&context_dir, args.file.as_deref())?;

    // Parse build args
    let build_args = parse_build_args(&args.build_arg)?;

    // Open image store
    let store = Arc::new(super::open_image_store()?);

    let platforms = parse_platforms(args.platform.as_deref())?;

    let config = a3s_box_runtime::BuildConfig {
        context_dir,
        dockerfile_path,
        tag: args.tag.clone(),
        build_args,
        quiet: args.quiet,
        platforms,
        target: args.target.clone(),
        no_cache: args.no_cache,
        metrics: None,
    };

    let result = a3s_box_runtime::oci::build::engine::build(config, store).await?;

    if args.quiet {
        println!("{}", result.digest);
    }

    Ok(())
}

/// Parse KEY=VALUE pairs into a HashMap.
fn parse_build_args(args: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("Invalid build arg (expected KEY=VALUE): {arg}"))?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

fn parse_platforms(
    platform: Option<&str>,
) -> Result<Vec<a3s_box_core::platform::Platform>, Box<dyn std::error::Error>> {
    let Some(platform) = platform else {
        return Ok(vec![]);
    };

    let platforms = a3s_box_core::platform::Platform::parse_list(platform)
        .map_err(|e| format!("Invalid --platform: {e}"))?;
    if platforms.len() > 1 {
        return Err(
            "Multi-platform builds are not implemented yet; pass a single --platform value".into(),
        );
    }
    if platforms.iter().any(|p| p.os != "linux") {
        return Err("Only linux target platforms are supported for builds".into());
    }

    Ok(platforms)
}

fn resolve_build_file(
    context_dir: &std::path::Path,
    file: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(file) = file {
        let path = PathBuf::from(file);
        let build_file = if path.is_absolute() {
            path
        } else {
            context_dir.join(path)
        };

        if build_file.exists() {
            return Ok(build_file);
        }

        return Err(format!("Build file not found at {}", build_file.display()).into());
    }

    for candidate in ["Dockerfile", "Containerfile"] {
        let path = context_dir.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(format!(
        "Build file not found: expected Dockerfile or Containerfile in {}",
        context_dir.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_build_args_valid() {
        let args = vec!["VERSION=1.0".to_string(), "DEBUG=true".to_string()];
        let result = parse_build_args(&args).unwrap();
        assert_eq!(result.get("VERSION"), Some(&"1.0".to_string()));
        assert_eq!(result.get("DEBUG"), Some(&"true".to_string()));
    }

    #[test]
    fn test_parse_build_args_empty() {
        let result = parse_build_args(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_build_args_invalid() {
        let args = vec!["NOEQUALS".to_string()];
        assert!(parse_build_args(&args).is_err());
    }

    #[test]
    fn test_parse_build_args_value_with_equals() {
        let args = vec!["URL=http://example.com?a=1".to_string()];
        let result = parse_build_args(&args).unwrap();
        assert_eq!(
            result.get("URL"),
            Some(&"http://example.com?a=1".to_string())
        );
    }

    #[test]
    fn test_parse_platforms_empty() {
        let result = parse_platforms(None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_platforms_single() {
        let result = parse_platforms(Some("linux/amd64")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "linux/amd64");
    }

    #[test]
    fn test_parse_platforms_rejects_multiple() {
        let err = parse_platforms(Some("linux/amd64,linux/arm64"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Multi-platform builds are not implemented yet"));
    }

    #[test]
    fn test_parse_platforms_rejects_non_linux() {
        let err = parse_platforms(Some("windows/amd64"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Only linux target platforms"));
    }

    #[test]
    fn test_resolve_build_file_prefers_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(tmp.path().join("Containerfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), None).unwrap();
        assert_eq!(path.file_name().unwrap(), "Dockerfile");
    }

    #[test]
    fn test_resolve_build_file_falls_back_to_containerfile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Containerfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), None).unwrap();
        assert_eq!(path.file_name().unwrap(), "Containerfile");
    }

    #[test]
    fn test_resolve_build_file_explicit_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Customfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), Some("Customfile")).unwrap();
        assert_eq!(path.file_name().unwrap(), "Customfile");
    }

    #[test]
    fn test_resolve_build_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_build_file(tmp.path(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Dockerfile or Containerfile"));
    }
}
