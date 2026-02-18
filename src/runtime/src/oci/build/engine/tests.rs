//! Tests for the build engine.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::super::utils::*;
    use std::collections::HashMap;

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/app", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/app", "src"), "/app/src");
    }

    #[test]
    fn test_resolve_path_root_workdir() {
        assert_eq!(resolve_path("/", "app"), "/app");
    }

    #[test]
    fn test_expand_args_braces() {
        let mut args = HashMap::new();
        args.insert("VERSION".to_string(), "3.19".to_string());
        assert_eq!(expand_args("alpine:${VERSION}", &args), "alpine:3.19");
    }

    #[test]
    fn test_expand_args_dollar() {
        let mut args = HashMap::new();
        args.insert("TAG".to_string(), "latest".to_string());
        assert_eq!(expand_args("image:$TAG", &args), "image:latest");
    }

    #[test]
    fn test_expand_args_no_match() {
        let args = HashMap::new();
        assert_eq!(expand_args("alpine:3.19", &args), "alpine:3.19");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }
}
