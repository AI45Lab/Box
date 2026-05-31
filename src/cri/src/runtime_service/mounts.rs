//! Read-only container mount materialization for the CRI runtime service.
//!
//! Copies CRI mount sources into a prepared rootfs, since microVM-backed
//! containers cannot bind-mount host paths directly.

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

fn remove_existing_mount_target(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

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

pub(super) fn materialize_readonly_container_mount(
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
    remove_existing_mount_target(&target).map_err(|e| {
        Status::internal(format!(
            "Failed to clear CRI mount target {}: {e}",
            target.display()
        ))
    })?;
    copy_mount_source(source, &target).map_err(|e| {
        Status::internal(format!(
            "Failed to materialize CRI read-only mount {} -> {}: {e}",
            source.display(),
            target.display()
        ))
    })
}
