//! `a3s-box build` command — Build an image from a Dockerfile.
//!
//! Parses a Dockerfile, pulls the base image, executes instructions,
//! and produces an OCI image stored in the local image store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct BuildArgs {
    /// Build context directory (contains Dockerfile and source files)
    #[arg(default_value = ".")]
    pub path: String,

    /// Name and optionally tag for the image (e.g., "myimage:latest")
    #[arg(short = 't', long = "tag")]
    pub tag: Option<String>,

    /// Path to Dockerfile (default: <PATH>/Dockerfile)
    #[arg(short = 'f', long = "file")]
    pub file: Option<String>,

    /// Set build-time variables (KEY=VALUE), can be repeated
    #[arg(long = "build-arg")]
    pub build_arg: Vec<String>,

    /// Suppress build output
    #[arg(short, long)]
    pub quiet: bool,

    /// Target platform(s) for multi-platform builds (e.g., "linux/amd64,linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,
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

    // Resolve Dockerfile path
    let dockerfile_path = match &args.file {
        Some(f) => {
            let p = PathBuf::from(f);
            if p.is_absolute() {
                p
            } else {
                context_dir.join(p)
            }
        }
        None => context_dir.join("Dockerfile"),
    };

    if !dockerfile_path.exists() {
        return Err(format!("Dockerfile not found at {}", dockerfile_path.display()).into());
    }

    // Parse build args
    let build_args = parse_build_args(&args.build_arg)?;

    // Open image store
    let store = Arc::new(super::open_image_store()?);

    // Parse target platforms
    let platforms = match &args.platform {
        Some(p) => a3s_box_core::platform::Platform::parse_list(p)
            .map_err(|e| format!("Invalid --platform: {e}"))?,
        None => vec![],
    };

    let config = a3s_box_runtime::BuildConfig {
        context_dir,
        dockerfile_path,
        tag: args.tag.clone(),
        build_args,
        quiet: args.quiet,
        platforms,
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
}
