//! Guest filesystem layout constants.
//!
//! Defines the directory structure inside the guest VM.

/// Working directory inside the guest VM.
pub const GUEST_WORKDIR: &str = "/workspace";

/// Guest filesystem layout.
#[derive(Debug, Clone)]
pub struct GuestLayout {
    /// Workspace mount point (virtio-fs).
    pub workspace_dir: &'static str,

    /// Temporary directory.
    pub tmp_dir: &'static str,

    /// Run directory for runtime files.
    pub run_dir: &'static str,
}

impl Default for GuestLayout {
    fn default() -> Self {
        Self {
            workspace_dir: "/workspace",
            tmp_dir: "/tmp",
            run_dir: "/run",
        }
    }
}

impl GuestLayout {
    /// Get the standard guest layout.
    pub fn standard() -> Self {
        Self::default()
    }

    /// Get all directories that need to be created in the rootfs.
    pub fn required_dirs(&self) -> Vec<&str> {
        vec![
            self.workspace_dir,
            self.tmp_dir,
            self.run_dir,
            "/dev",
            "/proc",
            "/sys",
            "/etc",
            "/var",
            "/var/tmp",
            "/var/log",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_layout_defaults() {
        let layout = GuestLayout::default();
        assert_eq!(layout.workspace_dir, "/workspace");
        assert_eq!(layout.tmp_dir, "/tmp");
        assert_eq!(layout.run_dir, "/run");
    }

    #[test]
    fn test_guest_layout_standard_equals_default() {
        let standard = GuestLayout::standard();
        let default = GuestLayout::default();
        assert_eq!(standard.workspace_dir, default.workspace_dir);
        assert_eq!(standard.tmp_dir, default.tmp_dir);
    }

    #[test]
    fn test_required_dirs_contains_all_layout_dirs() {
        let layout = GuestLayout::standard();
        let dirs = layout.required_dirs();

        assert!(dirs.contains(&layout.workspace_dir));
        assert!(dirs.contains(&layout.tmp_dir));
        assert!(dirs.contains(&layout.run_dir));
    }

    #[test]
    fn test_required_dirs_contains_system_dirs() {
        let layout = GuestLayout::standard();
        let dirs = layout.required_dirs();

        assert!(dirs.contains(&"/dev"));
        assert!(dirs.contains(&"/proc"));
        assert!(dirs.contains(&"/sys"));
        assert!(dirs.contains(&"/etc"));
        assert!(dirs.contains(&"/var"));
        assert!(dirs.contains(&"/var/tmp"));
        assert!(dirs.contains(&"/var/log"));
    }

    #[test]
    fn test_guest_workdir_constant() {
        assert_eq!(GUEST_WORKDIR, "/workspace");
        let layout = GuestLayout::default();
        assert_eq!(GUEST_WORKDIR, layout.workspace_dir);
    }

    #[test]
    fn test_guest_layout_clone() {
        let layout = GuestLayout::default();
        let cloned = layout.clone();
        assert_eq!(layout.workspace_dir, cloned.workspace_dir);
        assert_eq!(layout.tmp_dir, cloned.tmp_dir);
    }
}
