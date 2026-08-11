use std::{
    fs::{self, File},
    io::{self, Read},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    BoundPrivateDirectory, PrivateFileDigest, PrivateFileSnapshot, open_path_no_symlinks,
    staging_paths,
};

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and no preconditions.
    unsafe { libc::geteuid() }
}

pub(super) fn bound_descriptor_path(path: &Path) -> bool {
    use std::path::Component;

    matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [
            Component::RootDir,
            Component::Normal(proc),
            Component::Normal(this_process),
            Component::Normal(fd),
            Component::Normal(descriptor),
        ] if *proc == "proc"
            && *this_process == "self"
            && *fd == "fd"
            && descriptor.to_string_lossy().parse::<i32>().is_ok()
    )
}

pub(super) fn output_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

pub fn ensure_output_separate(output: &Path, inputs: &[(&Path, &str)]) -> Result<()> {
    let mut lock = output.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    let (staging, owner) = staging_paths(output);
    let candidates = [output, lock.as_path(), staging.as_path(), owner.as_path()];
    let resolved_candidates = candidates
        .iter()
        .map(|candidate| resolve_existing_ancestor(candidate))
        .collect::<Result<Vec<_>>>()?;
    for (input, label) in inputs {
        let resolved_input = fs::canonicalize(input)
            .with_context(|| format!("resolve {label} {}", input.display()))?;
        ensure!(
            resolved_candidates.iter().all(|candidate| {
                candidate != &resolved_input
                    && !candidate.starts_with(&resolved_input)
                    && !resolved_input.starts_with(candidate)
            }),
            "output, lock, and staging must be separate from {label} {}",
            resolved_input.display()
        );
        for candidate in candidates {
            use std::os::unix::fs::MetadataExt;
            let Ok(output_metadata) = fs::metadata(candidate) else {
                continue;
            };
            let input_metadata = fs::metadata(input)?;
            ensure!(
                output_metadata.dev() != input_metadata.dev()
                    || output_metadata.ino() != input_metadata.ino(),
                "output or lock must not alias {label} {}",
                resolved_input.display()
            );
        }
    }
    Ok(())
}

pub(super) fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_name().context("output must have a filename")?;
            Ok(fs::canonicalize(output_parent(path))?.join(name))
        }
        Err(error) => Err(error).with_context(|| format!("resolve output path {}", path.display())),
    }
}

pub fn private_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn read_bound_private_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_bound_private_file(path)?.bytes)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn read_bound_private_file(path: &Path) -> Result<PrivateFileSnapshot> {
    use std::os::unix::fs::MetadataExt;

    let (bytes, opened) = read_stable_private_file(path, |file| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    })?;
    Ok(PrivateFileSnapshot {
        bytes,
        device: opened.dev(),
        inode: opened.ino(),
    })
}

pub fn digest_bound_private_file(path: &Path) -> Result<PrivateFileDigest> {
    use std::os::unix::fs::MetadataExt;

    let ((sha256, bytes), opened) = read_stable_private_file(path, |file| {
        let mut hasher = Sha256::new();
        let bytes = io::copy(file, &mut hasher)?;
        Ok((format!("{:x}", hasher.finalize()), bytes))
    })?;
    Ok(PrivateFileDigest {
        sha256,
        bytes,
        device: opened.dev(),
        inode: opened.ino(),
    })
}

fn read_stable_private_file<T>(
    path: &Path,
    read: impl FnOnce(&mut File) -> Result<T>,
) -> Result<(T, fs::Metadata)> {
    use std::os::unix::fs::MetadataExt;

    let (parent, bound_path) = bind_private_parent(path)?;
    let mut file = open_path_no_symlinks(&bound_path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let opened = file.metadata()?;
    validate_private_metadata(&opened, path, false)?;
    let named = fs::symlink_metadata(&bound_path)?;
    ensure!(
        !named.file_type().is_symlink()
            && named.dev() == opened.dev()
            && named.ino() == opened.ino(),
        "private input changed while opening: {}",
        path.display()
    );
    let value = read(&mut file)?;
    let finished = file.metadata()?;
    let named_finished = fs::symlink_metadata(&bound_path)?;
    ensure!(
        !named_finished.file_type().is_symlink()
            && same_file_snapshot(&opened, &finished)
            && same_file_snapshot(&opened, &named_finished),
        "private input changed while reading: {}",
        path.display()
    );
    drop(parent);
    Ok((value, opened))
}

fn same_file_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.size() == right.size()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub fn open_private_bound_directory(path: &Path) -> Result<BoundPrivateDirectory> {
    let (parent, bound_path) = bind_private_parent(path)?;
    let directory = open_path_no_symlinks(&bound_path, libc::O_RDONLY | libc::O_DIRECTORY)
        .with_context(|| format!("open private directory {}", path.display()))?;
    bind_private_directory(parent, directory, &bound_path, path)
}

fn bind_private_directory(
    parent: File,
    directory: File,
    bound_path: &Path,
    display_path: &Path,
) -> Result<BoundPrivateDirectory> {
    use std::os::unix::fs::MetadataExt;

    let opened = directory.metadata()?;
    validate_private_metadata(&opened, display_path, true)?;
    let named = fs::symlink_metadata(bound_path)?;
    ensure!(
        !named.file_type().is_symlink()
            && named.dev() == opened.dev()
            && named.ino() == opened.ino(),
        "private directory changed while opening: {}",
        display_path.display()
    );
    let bound_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    Ok(BoundPrivateDirectory {
        _parent: parent,
        _directory: directory,
        path: bound_path,
    })
}

pub(super) fn bind_private_parent(path: &Path) -> Result<(File, PathBuf)> {
    use std::os::unix::fs::MetadataExt;

    let requested = output_parent(path);
    let parent = open_path_no_symlinks(requested, libc::O_RDONLY | libc::O_DIRECTORY)
        .with_context(|| format!("open private input parent {}", requested.display()))?;
    let opened = parent.metadata()?;
    ensure!(
        opened.is_dir() && opened.uid() == effective_uid() && opened.mode() & 0o022 == 0,
        "private input parent must be owner-controlled and non-writable: {}",
        requested.display()
    );
    if !bound_descriptor_path(requested) {
        let named = fs::symlink_metadata(requested)?;
        ensure!(
            !named.file_type().is_symlink()
                && named.dev() == opened.dev()
                && named.ino() == opened.ino(),
            "private input parent changed while opening: {}",
            requested.display()
        );
    }
    let name = path
        .file_name()
        .or_else(|| (path == Path::new(".")).then_some(path.as_os_str()))
        .context("private input must have a filename")?;
    let bound_parent = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    Ok((parent, bound_parent.join(name)))
}

impl BoundPrivateDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[expect(
    clippy::verbose_bit_mask,
    reason = "the explicit Unix mode mask mirrors the persisted private-file contract"
)]
pub(super) fn validate_private_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    directory: bool,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "private input has the wrong file type: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_uid(),
        "private input is not owned by the current user: {}",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "private input is not owner-only: {}",
        path.display()
    );
    Ok(())
}
