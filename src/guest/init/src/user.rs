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

/// Resolve a named user (e.g. CRI `RunAsUserName` "nobody") to a numeric
/// `"uid:gid"` string by looking it up in the container's `<rootfs>/etc/passwd`.
///
/// Returns `None` when no resolution is needed or possible — the user is
/// numeric / `root` (handled by [`parse_process_user`]), the passwd file is
/// missing, or the name is not found — leaving `parse_process_user` to accept
/// the numeric form or reject the unresolved name. An explicit numeric group
/// suffix (`name:gid`) is preserved.
pub fn resolve_named_user(user: &str, rootfs: &str) -> Option<String> {
    let user = user.trim();
    let (name, group_suffix) = match user.split_once(':') {
        Some((name, group)) => (name, Some(group)),
        None => (user, None),
    };
    if name.is_empty() || name == "root" || name.parse::<u32>().is_ok() {
        return None;
    }
    let passwd = std::fs::read_to_string(std::path::Path::new(rootfs).join("etc/passwd")).ok()?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 4 && fields[0] == name {
            let uid: u32 = fields[2].parse().ok()?;
            let gid: u32 = fields[3].parse().ok()?;
            return Some(match group_suffix {
                Some(group) => format!("{uid}:{group}"),
                None => format!("{uid}:{gid}"),
            });
        }
    }
    None
}

/// Resolve the supplementary groups a user belongs to via the container's
/// `<rootfs>/etc/passwd` + `<rootfs>/etc/group`, like glibc `initgroups`.
///
/// `user` is the original runtime user string (name, `uid`, or `uid:gid`); it
/// selects the username matched against `/etc/group` member lists — used as-is
/// when named, otherwise reverse-mapped from the uid via `/etc/passwd`. The
/// result is the user's primary gid (the explicit `:gid`, else the passwd
/// entry's gid) followed by every group whose member list names the user,
/// de-duplicated in a stable order. Missing files yield only the primary gid
/// (matching a runtime that cannot enumerate image groups).
///
/// Pure file reads with no allocation constraints — call before `fork`, never
/// in the post-fork child.
pub fn resolve_image_groups(
    rootfs: &str,
    uid: u32,
    explicit_gid: Option<u32>,
    user: &str,
) -> Vec<u32> {
    let name_part = user.trim().split(':').next().unwrap_or("").trim();
    let is_named =
        !name_part.is_empty() && name_part != "root" && name_part.parse::<u32>().is_err();
    let passwd_entry = passwd_entry_for_uid(rootfs, uid);
    let username: Option<String> = if is_named {
        Some(name_part.to_string())
    } else {
        passwd_entry.as_ref().map(|(name, _)| name.clone())
    };

    let mut groups: Vec<u32> = Vec::new();
    if let Some(gid) = explicit_gid.or_else(|| passwd_entry.as_ref().map(|(_, gid)| *gid)) {
        groups.push(gid);
    }
    if let Some(username) = username.as_deref() {
        if let Ok(group_file) =
            std::fs::read_to_string(std::path::Path::new(rootfs).join("etc/group"))
        {
            for line in group_file.lines() {
                let fields: Vec<&str> = line.split(':').collect();
                if fields.len() < 4 {
                    continue;
                }
                let Ok(gid) = fields[2].parse::<u32>() else {
                    continue;
                };
                if fields[3].split(',').any(|member| member.trim() == username) {
                    groups.push(gid);
                }
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    groups.retain(|gid| seen.insert(*gid));
    groups
}

/// The primary gid recorded for `uid` in `<rootfs>/etc/passwd`, if present.
///
/// Used to default a container's primary group to the image's passwd entry when
/// the runtime specifies a user (`RunAsUser`) but no group (`RunAsGroup`).
pub fn primary_gid_for_uid(rootfs: &str, uid: u32) -> Option<u32> {
    passwd_entry_for_uid(rootfs, uid).map(|(_, gid)| gid)
}

/// Look up a user's name and primary gid by uid in `<rootfs>/etc/passwd`.
fn passwd_entry_for_uid(rootfs: &str, uid: u32) -> Option<(String, u32)> {
    let passwd = std::fs::read_to_string(std::path::Path::new(rootfs).join("etc/passwd")).ok()?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 4 && fields[2].parse::<u32>().ok() == Some(uid) {
            if let Ok(gid) = fields[3].parse::<u32>() {
                return Some((fields[0].to_string(), gid));
            }
        }
    }
    None
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

    fn write_rootfs(passwd: &str, group: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/passwd"), passwd).unwrap();
        std::fs::write(dir.path().join("etc/group"), group).unwrap();
        dir
    }

    #[test]
    fn test_resolve_image_groups_numeric_user_via_passwd() {
        // A numeric uid is reverse-mapped to its name, whose /etc/group
        // memberships become supplementary groups (primary gid included).
        let dir = write_rootfs(
            "root:x:0:0:root:/root:/sh\ntester:x:1000:1000:tester:/home/tester:/sh\n",
            "root:x:0:\ntester:x:1000:\nextra:x:2000:tester\nadmins:x:2001:tester,root\n",
        );
        let rootfs = dir.path().to_str().unwrap();
        let mut groups = resolve_image_groups(rootfs, 1000, None, "1000");
        groups.sort();
        assert_eq!(groups, vec![1000, 2000, 2001]);
    }

    #[test]
    fn test_resolve_image_groups_named_user() {
        let dir = write_rootfs(
            "tester:x:1000:1000:tester:/home/tester:/sh\n",
            "extra:x:2000:tester\nother:x:3000:someoneelse\n",
        );
        let rootfs = dir.path().to_str().unwrap();
        // Named user with an explicit primary gid; only matching member groups.
        let mut groups = resolve_image_groups(rootfs, 1000, Some(1000), "tester:1000");
        groups.sort();
        assert_eq!(groups, vec![1000, 2000]);
    }

    #[test]
    fn test_resolve_image_groups_uid_absent_from_passwd() {
        // A uid with no passwd entry and no name yields nothing to add beyond an
        // explicit primary gid (matches a runtime that cannot enumerate groups).
        let dir = write_rootfs("root:x:0:0:root:/root:/sh\n", "root:x:0:\nfoo:x:5000:bar\n");
        let rootfs = dir.path().to_str().unwrap();
        assert_eq!(
            resolve_image_groups(rootfs, 4242, None, "4242"),
            Vec::<u32>::new()
        );
        assert_eq!(resolve_image_groups(rootfs, 4242, Some(7), "4242"), vec![7]);
    }

    #[test]
    fn test_resolve_image_groups_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap();
        // No /etc/passwd or /etc/group: only an explicit primary gid survives.
        assert_eq!(
            resolve_image_groups(rootfs, 1000, Some(1000), "1000"),
            vec![1000]
        );
    }
}
