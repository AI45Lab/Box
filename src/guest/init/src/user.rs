//! Numeric user parsing and application for guest child processes.

/// User/group identity for a process spawned by guest init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUser {
    /// Numeric user ID.
    pub uid: u32,
    /// Optional numeric group ID.
    pub gid: Option<u32>,
}

/// Parse a runtime user string.
///
/// Supported formats are `root`, `uid`, `root:root`, and `uid:gid`. Other
/// named users are rejected because guest-init does not do `/etc/passwd`
/// lookups yet.
pub fn parse_process_user(user: Option<&str>) -> Result<Option<ProcessUser>, String> {
    let Some(user) = user else {
        return Ok(None);
    };
    let user = user.trim();
    if user.is_empty() {
        return Err("user must not be empty".to_string());
    }

    let parts: Vec<&str> = user.split(':').collect();
    if parts.len() > 2 {
        return Err(format!(
            "invalid user '{user}' (expected root, UID, or UID:GID)"
        ));
    }

    let uid = parse_user_part(parts[0], "user", user)?;
    let gid = parts
        .get(1)
        .map(|part| parse_user_part(part, "group", user))
        .transpose()?;

    Ok(Some(ProcessUser { uid, gid }))
}

fn parse_user_part(part: &str, label: &str, original: &str) -> Result<u32, String> {
    if part.is_empty() {
        return Err(format!(
            "invalid user '{original}' ({label} component is empty)"
        ));
    }
    if part == "root" {
        return Ok(0);
    }
    part.parse::<u32>().map_err(|_| {
        format!("named {label} '{part}' is not supported yet; use root or a numeric UID[:GID]")
    })
}

impl ProcessUser {
    /// Apply the parsed UID/GID to the current child process.
    #[cfg(unix)]
    pub fn apply(self) -> std::io::Result<()> {
        if let Some(gid) = self.gid {
            let ret = unsafe { libc::setgid(gid as libc::gid_t) };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        let ret = unsafe { libc::setuid(self.uid as libc::uid_t) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Non-Unix guest-init builds are development stubs.
    #[cfg(not(unix))]
    pub fn apply(self) -> std::io::Result<()> {
        let _ = self;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_process_user_none() {
        assert_eq!(parse_process_user(None).unwrap(), None);
    }

    #[test]
    fn test_parse_process_user_numeric() {
        assert_eq!(
            parse_process_user(Some("1000")).unwrap(),
            Some(ProcessUser {
                uid: 1000,
                gid: None
            })
        );
        assert_eq!(
            parse_process_user(Some("1000:1001")).unwrap(),
            Some(ProcessUser {
                uid: 1000,
                gid: Some(1001)
            })
        );
    }

    #[test]
    fn test_parse_process_user_root_alias() {
        assert_eq!(
            parse_process_user(Some("root:root")).unwrap(),
            Some(ProcessUser {
                uid: 0,
                gid: Some(0)
            })
        );
    }

    #[test]
    fn test_parse_process_user_rejects_names() {
        let err = parse_process_user(Some("node")).unwrap_err();
        assert!(err.contains("named user"));
    }

    #[test]
    fn test_parse_process_user_rejects_empty_components() {
        assert!(parse_process_user(Some(":1000")).is_err());
        assert!(parse_process_user(Some("1000:")).is_err());
    }
}
