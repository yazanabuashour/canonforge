use std::{
    fs::File,
    io::BufWriter,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

mod blob;
mod filesystem;
mod input;
mod publication;
#[cfg(test)]
mod tests;

pub use blob::publish_content_addressed_blob;
pub use filesystem::{directory_tree_bytes, rename_directory_no_replace, sync_directory};
use filesystem::{private_parent, rename_no_replace};
#[cfg(test)]
use input::bind_private_parent;
use input::{
    bound_descriptor_path, output_parent, resolve_existing_ancestor, validate_private_metadata,
};
pub use input::{
    digest_bound_private_file, effective_uid, ensure_output_separate, open_private_bound_directory,
    private_mode, read_bound_private_file, read_bound_private_json,
};
use publication::{ensure_outside_evidence_package, staging_paths};
pub use publication::{ensure_private_relative_directory, private_staging_writer};

pub struct PrivateWriter {
    inner: BufWriter<NamedTempFile>,
    destination: BoundOutput,
}

pub struct PrivateDirectory {
    staging: PathBuf,
    owner: PathBuf,
    target: BoundOutput,
    _lock: TargetLock,
    finished: bool,
}

pub struct TargetLock {
    _file: File,
}

pub struct BoundOutput {
    parent_handle: File,
    requested_parent: PathBuf,
    parent: PathBuf,
    target: PathBuf,
    resolved: PathBuf,
}

pub struct BoundPrivateDirectory {
    _parent: File,
    _directory: File,
    path: PathBuf,
}

pub struct PrivateFileSnapshot {
    pub bytes: Vec<u8>,
    pub device: u64,
    pub inode: u64,
}

pub struct PrivateFileDigest {
    pub sha256: String,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "libc flag and mode widths are checked against the platform ABI at this boundary"
)]
pub fn open_path_no_symlinks(path: &Path, flags: i32) -> Result<File> {
    use std::{ffi::CString, os::fd::FromRawFd, os::unix::ffi::OsStrExt, path::Component};

    let components = path.components().collect::<Vec<_>>();
    let bound_descriptor = match components.as_slice() {
        [
            Component::RootDir,
            Component::Normal(proc),
            Component::Normal(this_process),
            Component::Normal(fd),
            Component::Normal(descriptor),
            rest @ ..,
        ] if *proc == "proc" && *this_process == "self" && *fd == "fd" => Some((
            descriptor
                .to_str()
                .context("bound descriptor is not UTF-8")?
                .parse::<i32>()
                .context("bound descriptor is not numeric")?,
            rest.iter().collect::<PathBuf>(),
        )),
        _ => None,
    };
    let root;
    let (root_descriptor, relative) = if let Some((descriptor, relative)) = bound_descriptor {
        if relative.as_os_str().is_empty() {
            // SAFETY: descriptor names an already-open process descriptor; fcntl duplicates it.
            let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
            if duplicate < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("duplicate bound protected descriptor");
            }
            // SAFETY: F_DUPFD_CLOEXEC returns one newly owned descriptor.
            return Ok(unsafe { File::from_raw_fd(duplicate) });
        }
        (descriptor, relative)
    } else if path.is_absolute() {
        root = File::open("/")?;
        (root.as_raw_fd(), path.strip_prefix("/")?.to_owned())
    } else {
        root = File::open(".")?;
        (root.as_raw_fd(), path.to_owned())
    };
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &relative
    };
    let relative = CString::new(relative.as_os_str().as_bytes())
        .context("protected path contains a NUL byte")?;
    // SAFETY: libc::open_how contains only integer fields and accepts all-zero initialization.
    let mut how = unsafe { std::mem::zeroed::<libc::open_how>() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: root and relative remain valid for the syscall, and libc defines the ABI structure.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_descriptor,
            relative.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open protected path without symlinks {}", path.display()));
    }
    // SAFETY: a successful openat2 returns one newly owned file descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor as i32) })
}
