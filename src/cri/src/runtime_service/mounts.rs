//! Container mount materialization for the CRI runtime service.
//!
//! On Linux, bind-mounts each CRI mount source onto its target inside the
//! container rootfs. The rootfs is shared into the pod microVM over virtio-fs,
//! so in-container writes propagate live to the host source — satisfying the
//! Docker/CRI mount contract (emptyDir sharing between a pod's containers,
//! hostPath persistence). Teardown MUST lazy-unmount these binds before removing
//! the rootfs (see `unmount_submounts_under` in `service_ops`), or
//! `remove_dir_all` would delete the host source through the live mount.
//! Non-Linux dev builds and unit tests fall back to copying the source in
//! (no live writeback).

use std::path::{Path, PathBuf};

use tonic::Status;

use crate::container::ContainerMount;

fn container_path_inside_rootfs(rootfs: &Path, container_path: &str) -> Result<PathBuf, Status> {
    let path = Path::new(container_path);
    if !path.is_absolute() {
        return Err(Status::invalid_argument(format!(
            "CRI mount container_path must be absolute: {container_path}"
        )));
    }

    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => relative.push(part),
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(Status::invalid_argument(format!(
                    "CRI mount container_path must not escape the rootfs: {container_path}"
                )));
            }
        }
    }

    if relative.as_os_str().is_empty() {
        return Err(Status::invalid_argument(
            "CRI mount container_path must not be the rootfs",
        ));
    }

    Ok(rootfs.join(relative))
}

/// Make the bind target exist with the same kind as the source: `mount --bind`
/// requires the target to already be present — a directory for a directory
/// source, a regular file for a file source. An existing target of the wrong
/// kind is replaced.
#[cfg(all(target_os = "linux", not(test)))]
fn ensure_bind_target(source: &Path, target: &Path) -> std::io::Result<()> {
    let src_is_dir = std::fs::metadata(source)?.is_dir();
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() == src_is_dir => return Ok(()),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(target)?,
        Ok(_) => std::fs::remove_file(target)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if src_is_dir {
        std::fs::create_dir_all(target)?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(target)?;
    }
    Ok(())
}

/// Bind-mount the host source onto the target inside the (virtio-fs-shared)
/// container rootfs so in-container writes propagate live to the host source.
#[cfg(all(target_os = "linux", not(test)))]
fn bind_mount_source(source: &Path, target: &Path, readonly: bool) -> Result<(), Status> {
    ensure_bind_target(source, target).map_err(|e| {
        Status::internal(format!(
            "Failed to prepare CRI mount target {}: {e}",
            target.display()
        ))
    })?;
    let ok = std::process::Command::new("mount")
        .arg("--bind")
        .arg(source)
        .arg(target)
        .status()
        .map_err(|e| Status::internal(format!("Failed to run mount --bind: {e}")))?
        .success();
    if !ok {
        return Err(Status::internal(format!(
            "mount --bind {} -> {} failed",
            source.display(),
            target.display()
        )));
    }
    if readonly {
        // A read-only bind needs a second remount. Fail CLOSED if it fails: a
        // volume the pod marked readOnly (a secret/configMap, or a readOnly
        // hostPath) must never be silently exposed writable. Tear down the
        // read-write bind we just made and surface the error.
        let ro = std::process::Command::new("mount")
            .arg("-o")
            .arg("remount,bind,ro")
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ro {
            let _ = std::process::Command::new("umount")
                .arg("-l")
                .arg(target)
                .status();
            return Err(Status::internal(format!(
                "Failed to remount CRI mount {} read-only (refusing to expose a \
                 readOnly volume as writable)",
                target.display()
            )));
        }
    }
    Ok(())
}

/// Copy fallback (non-Linux dev builds and unit tests): no live writeback.
#[cfg(any(not(target_os = "linux"), test))]
fn remove_existing_mount_target(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(any(not(target_os = "linux"), test))]
fn copy_mount_source(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_mount_source(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, target)?;
    }

    Ok(())
}

pub(super) fn materialize_container_mount(
    rootfs: &Path,
    mount: &ContainerMount,
) -> Result<(), Status> {
    let source = Path::new(&mount.host_path);
    if !source.exists() {
        return Err(Status::failed_precondition(format!(
            "CRI mount host_path does not exist: {}",
            mount.host_path
        )));
    }

    let target = container_path_inside_rootfs(rootfs, &mount.container_path)?;

    // Linux production: bind-mount so writes propagate to the host source (the
    // rootfs is virtio-fs-shared into the pod VM). Teardown lazy-unmounts these
    // before removing the rootfs — see `unmount_submounts_under` — to avoid
    // deleting the host source through the live mount.
    #[cfg(all(target_os = "linux", not(test)))]
    {
        bind_mount_source(source, &target, mount.readonly)
    }

    // Non-Linux dev builds and unit tests: copy in (no live writeback).
    #[cfg(any(not(target_os = "linux"), test))]
    {
        if !mount.readonly {
            tracing::warn!(
                host_path = %mount.host_path,
                container_path = %mount.container_path,
                "Writable CRI mount copied (no live writeback on this build)"
            );
        }
        remove_existing_mount_target(&target).map_err(|e| {
            Status::internal(format!(
                "Failed to clear CRI mount target {}: {e}",
                target.display()
            ))
        })?;
        copy_mount_source(source, &target).map_err(|e| {
            Status::internal(format!(
                "Failed to materialize CRI mount {} -> {}: {e}",
                source.display(),
                target.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(host_path: &Path, container_path: &str, readonly: bool) -> ContainerMount {
        ContainerMount {
            container_path: container_path.to_string(),
            host_path: host_path.to_string_lossy().to_string(),
            readonly,
            selinux_relabel: false,
            propagation: 0,
        }
    }

    #[test]
    fn container_path_inside_rootfs_strips_root_and_curdir() {
        let rootfs = Path::new("/tmp/rootfs");

        let path = container_path_inside_rootfs(rootfs, "/var/./log/app.log").unwrap();

        assert_eq!(path, rootfs.join("var").join("log").join("app.log"));
    }

    #[test]
    fn container_path_inside_rootfs_rejects_relative_path() {
        let err = container_path_inside_rootfs(Path::new("/tmp/rootfs"), "var/log")
            .expect_err("relative container mount targets must be rejected");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("must be absolute"));
    }

    #[test]
    fn container_path_inside_rootfs_rejects_parent_escape() {
        let err = container_path_inside_rootfs(Path::new("/tmp/rootfs"), "/var/../host")
            .expect_err("parent traversal must be rejected");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("must not escape"));
    }

    #[test]
    fn container_path_inside_rootfs_rejects_root_mount() {
        let err = container_path_inside_rootfs(Path::new("/tmp/rootfs"), "/")
            .expect_err("mounting over the whole rootfs must be rejected");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("must not be the rootfs"));
    }

    #[test]
    fn materialize_container_mount_copies_file_source() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let source = tmp.path().join("host").join("config.txt");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "hello from host\n").unwrap();

        materialize_container_mount(&rootfs, &mount(&source, "/etc/config.txt", true)).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc").join("config.txt")).unwrap(),
            "hello from host\n"
        );
    }

    #[test]
    fn materialize_container_mount_copies_directory_source_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let source = tmp.path().join("host").join("data");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("top.txt"), "top").unwrap();
        std::fs::write(source.join("nested").join("leaf.txt"), "leaf").unwrap();

        materialize_container_mount(&rootfs, &mount(&source, "/data", false)).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("data").join("top.txt")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("data").join("nested").join("leaf.txt")).unwrap(),
            "leaf"
        );
    }

    #[test]
    fn materialize_container_mount_replaces_wrong_target_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("var").join("lib").join("data");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "old file target").unwrap();

        let source = tmp.path().join("host").join("data");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("new.txt"), "new dir source").unwrap();

        materialize_container_mount(&rootfs, &mount(&source, "/var/lib/data", false)).unwrap();

        assert!(target.is_dir());
        assert_eq!(
            std::fs::read_to_string(target.join("new.txt")).unwrap(),
            "new dir source"
        );
    }

    #[test]
    fn materialize_container_mount_rejects_missing_host_path() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let missing = tmp.path().join("missing");

        let err = materialize_container_mount(&rootfs, &mount(&missing, "/data", false))
            .expect_err("missing CRI host_path must fail closed");

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("host_path does not exist"));
    }
}
