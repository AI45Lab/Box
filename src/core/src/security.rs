//! Security configuration for guest process hardening.
//!
//! Parses `--security-opt` values and capability lists into an actionable
//! security profile that guest-init applies before exec.

use serde::{Deserialize, Serialize};

/// Seccomp filter mode for the guest process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeccompMode {
    /// Apply the default seccomp profile (blocks dangerous syscalls).
    #[default]
    Default,
    /// Disable seccomp filtering entirely.
    Unconfined,
    /// Use a custom seccomp profile from a JSON file path.
    Custom(String),
}

/// Parsed security configuration for guest process enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Seccomp filter mode.
    pub seccomp: SeccompMode,
    /// Set PR_SET_NO_NEW_PRIVS before exec.
    pub no_new_privileges: bool,
    /// Linux capabilities to add.
    pub cap_add: Vec<String>,
    /// Linux capabilities to drop.
    pub cap_drop: Vec<String>,
    /// Privileged mode (disables all restrictions).
    pub privileged: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            seccomp: SeccompMode::Default,
            no_new_privileges: true, // secure by default
            cap_add: vec![],
            cap_drop: vec![],
            privileged: false,
        }
    }
}

impl SecurityConfig {
    /// Validate that the security configuration can be enforced at runtime.
    ///
    /// Returns an error if custom seccomp profiles are specified, since they
    /// are not yet supported and would silently fall through to no filtering.
    pub fn validate(&self) -> Result<(), String> {
        if let SeccompMode::Custom(path) = &self.seccomp {
            return Err(format!(
                "custom seccomp profile '{}' is not supported; \
                 use seccomp=default or seccomp=unconfined",
                path
            ));
        }
        Ok(())
    }

    /// Parse security config from CLI-style options.
    ///
    /// Accepts the same format as Docker:
    /// - `seccomp=unconfined` — disable seccomp
    /// - `seccomp=<path>` — custom profile
    /// - `no-new-privileges` or `no-new-privileges=true` — enable (default)
    /// - `no-new-privileges=false` — disable
    pub fn from_options(
        security_opt: &[String],
        cap_add: &[String],
        cap_drop: &[String],
        privileged: bool,
    ) -> Self {
        if privileged {
            return Self {
                seccomp: SeccompMode::Unconfined,
                no_new_privileges: false,
                cap_add: vec!["ALL".to_string()],
                cap_drop: vec![],
                privileged: true,
            };
        }

        let mut config = Self {
            cap_add: cap_add.to_vec(),
            cap_drop: cap_drop.to_vec(),
            ..Self::default()
        };

        for opt in security_opt {
            let opt = opt.trim();
            if let Some(value) = opt.strip_prefix("seccomp=") {
                config.seccomp = if value == "unconfined" {
                    SeccompMode::Unconfined
                } else {
                    SeccompMode::Custom(value.to_string())
                };
            } else if opt == "no-new-privileges" || opt == "no-new-privileges=true" {
                config.no_new_privileges = true;
            } else if opt == "no-new-privileges=false" {
                config.no_new_privileges = false;
            }
        }

        config
    }

    /// Encode as environment variables for passing to guest-init.
    ///
    /// Returns a list of (key, value) pairs with `A3S_SEC_*` prefix.
    pub fn to_env_vars(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();

        // Seccomp mode
        let seccomp_value = match &self.seccomp {
            SeccompMode::Default => "default".to_string(),
            SeccompMode::Unconfined => "unconfined".to_string(),
            SeccompMode::Custom(path) => format!("custom:{}", path),
        };
        env.push(("A3S_SEC_SECCOMP".to_string(), seccomp_value));

        // No new privileges
        env.push((
            "A3S_SEC_NO_NEW_PRIVS".to_string(),
            if self.no_new_privileges { "1" } else { "0" }.to_string(),
        ));

        // Privileged
        if self.privileged {
            env.push(("A3S_SEC_PRIVILEGED".to_string(), "1".to_string()));
        }

        // Capabilities
        if !self.cap_add.is_empty() {
            env.push(("A3S_SEC_CAP_ADD".to_string(), self.cap_add.join(",")));
        }
        if !self.cap_drop.is_empty() {
            env.push(("A3S_SEC_CAP_DROP".to_string(), self.cap_drop.join(",")));
        }

        env
    }

    /// Parse from guest-init environment variables.
    pub fn from_env_vars() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("A3S_SEC_PRIVILEGED") {
            if val == "1" {
                return Self {
                    seccomp: SeccompMode::Unconfined,
                    no_new_privileges: false,
                    cap_add: vec!["ALL".to_string()],
                    cap_drop: vec![],
                    privileged: true,
                };
            }
        }

        if let Ok(val) = std::env::var("A3S_SEC_SECCOMP") {
            config.seccomp = if val == "unconfined" {
                SeccompMode::Unconfined
            } else if val == "default" {
                SeccompMode::Default
            } else if let Some(path) = val.strip_prefix("custom:") {
                SeccompMode::Custom(path.to_string())
            } else {
                SeccompMode::Default
            };
        }

        if let Ok(val) = std::env::var("A3S_SEC_NO_NEW_PRIVS") {
            config.no_new_privileges = val == "1";
        }

        if let Ok(val) = std::env::var("A3S_SEC_CAP_ADD") {
            config.cap_add = val.split(',').map(|s| s.trim().to_string()).collect();
        }

        if let Ok(val) = std::env::var("A3S_SEC_CAP_DROP") {
            config.cap_drop = val.split(',').map(|s| s.trim().to_string()).collect();
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_security_config_default() {
        let config = SecurityConfig::default();
        assert_eq!(config.seccomp, SeccompMode::Default);
        assert!(config.no_new_privileges);
        assert!(config.cap_add.is_empty());
        assert!(config.cap_drop.is_empty());
        assert!(!config.privileged);
    }

    #[test]
    fn test_seccomp_mode_default() {
        assert_eq!(SeccompMode::default(), SeccompMode::Default);
    }

    #[test]
    fn test_from_options_empty() {
        let config = SecurityConfig::from_options(&[], &[], &[], false);
        assert_eq!(config.seccomp, SeccompMode::Default);
        assert!(config.no_new_privileges);
        assert!(!config.privileged);
    }

    #[test]
    fn test_from_options_seccomp_unconfined() {
        let opts = vec!["seccomp=unconfined".to_string()];
        let config = SecurityConfig::from_options(&opts, &[], &[], false);
        assert_eq!(config.seccomp, SeccompMode::Unconfined);
    }

    #[test]
    fn test_from_options_seccomp_custom() {
        let opts = vec!["seccomp=/path/to/profile.json".to_string()];
        let config = SecurityConfig::from_options(&opts, &[], &[], false);
        assert_eq!(
            config.seccomp,
            SeccompMode::Custom("/path/to/profile.json".to_string())
        );
    }

    #[test]
    fn test_from_options_no_new_privileges() {
        let opts = vec!["no-new-privileges".to_string()];
        let config = SecurityConfig::from_options(&opts, &[], &[], false);
        assert!(config.no_new_privileges);
    }

    #[test]
    fn test_from_options_no_new_privileges_false() {
        let opts = vec!["no-new-privileges=false".to_string()];
        let config = SecurityConfig::from_options(&opts, &[], &[], false);
        assert!(!config.no_new_privileges);
    }

    #[test]
    fn test_from_options_privileged() {
        let config = SecurityConfig::from_options(&[], &[], &[], true);
        assert!(config.privileged);
        assert_eq!(config.seccomp, SeccompMode::Unconfined);
        assert!(!config.no_new_privileges);
        assert_eq!(config.cap_add, vec!["ALL"]);
    }

    #[test]
    fn test_from_options_capabilities() {
        let cap_add = vec!["NET_ADMIN".to_string(), "SYS_PTRACE".to_string()];
        let cap_drop = vec!["NET_RAW".to_string()];
        let config = SecurityConfig::from_options(&[], &cap_add, &cap_drop, false);
        assert_eq!(config.cap_add, vec!["NET_ADMIN", "SYS_PTRACE"]);
        assert_eq!(config.cap_drop, vec!["NET_RAW"]);
    }

    #[test]
    fn test_to_env_vars_default() {
        let config = SecurityConfig::default();
        let env = config.to_env_vars();
        assert!(env.contains(&("A3S_SEC_SECCOMP".to_string(), "default".to_string())));
        assert!(env.contains(&("A3S_SEC_NO_NEW_PRIVS".to_string(), "1".to_string())));
        // No cap_add/cap_drop/privileged env vars when empty
        assert!(!env.iter().any(|(k, _)| k == "A3S_SEC_CAP_ADD"));
        assert!(!env.iter().any(|(k, _)| k == "A3S_SEC_CAP_DROP"));
        assert!(!env.iter().any(|(k, _)| k == "A3S_SEC_PRIVILEGED"));
    }

    #[test]
    fn test_to_env_vars_privileged() {
        let config = SecurityConfig::from_options(&[], &[], &[], true);
        let env = config.to_env_vars();
        assert!(env.contains(&("A3S_SEC_SECCOMP".to_string(), "unconfined".to_string())));
        assert!(env.contains(&("A3S_SEC_NO_NEW_PRIVS".to_string(), "0".to_string())));
        assert!(env.contains(&("A3S_SEC_PRIVILEGED".to_string(), "1".to_string())));
        assert!(env.contains(&("A3S_SEC_CAP_ADD".to_string(), "ALL".to_string())));
    }

    #[test]
    fn test_to_env_vars_with_caps() {
        let cap_add = vec!["NET_ADMIN".to_string()];
        let cap_drop = vec!["ALL".to_string()];
        let config = SecurityConfig::from_options(&[], &cap_add, &cap_drop, false);
        let env = config.to_env_vars();
        assert!(env.contains(&("A3S_SEC_CAP_ADD".to_string(), "NET_ADMIN".to_string())));
        assert!(env.contains(&("A3S_SEC_CAP_DROP".to_string(), "ALL".to_string())));
    }

    #[test]
    fn test_env_vars_roundtrip() {
        let original = SecurityConfig::from_options(
            &["seccomp=unconfined".to_string()],
            &["NET_ADMIN".to_string(), "SYS_PTRACE".to_string()],
            &["NET_RAW".to_string()],
            false,
        );
        let env = original.to_env_vars();

        // Simulate setting env vars
        for (key, value) in &env {
            std::env::set_var(key, value);
        }

        let parsed = SecurityConfig::from_env_vars();
        assert_eq!(parsed.seccomp, SeccompMode::Unconfined);
        assert!(parsed.no_new_privileges); // default from original
        assert_eq!(parsed.cap_add, vec!["NET_ADMIN", "SYS_PTRACE"]);
        assert_eq!(parsed.cap_drop, vec!["NET_RAW"]);
        assert!(!parsed.privileged);

        // Clean up env vars
        for (key, _) in &env {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_security_config_serde_roundtrip() {
        let config = SecurityConfig {
            seccomp: SeccompMode::Custom("/my/profile.json".to_string()),
            no_new_privileges: false,
            cap_add: vec!["NET_ADMIN".to_string()],
            cap_drop: vec!["ALL".to_string()],
            privileged: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SecurityConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.seccomp,
            SeccompMode::Custom("/my/profile.json".to_string())
        );
        assert!(!parsed.no_new_privileges);
        assert_eq!(parsed.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(parsed.cap_drop, vec!["ALL"]);
    }

    // --- SecurityConfig::validate tests ---

    #[test]
    fn test_validate_default_ok() {
        let config = SecurityConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_unconfined_ok() {
        let config = SecurityConfig::from_options(&["seccomp=unconfined".to_string()], &[], &[], false);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_custom_rejected() {
        let config = SecurityConfig::from_options(
            &["seccomp=/path/to/profile.json".to_string()],
            &[],
            &[],
            false,
        );
        let err = config.validate().unwrap_err();
        assert!(err.contains("custom seccomp profile"));
        assert!(err.contains("not supported"));
    }

    #[test]
    fn test_validate_privileged_ok() {
        let config = SecurityConfig::from_options(&[], &[], &[], true);
        assert!(config.validate().is_ok());
    }
}
