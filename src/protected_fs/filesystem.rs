use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use walkdir::WalkDir;

use super::{
    bound_descriptor_path, effective_uid, ensure_outside_evidence_package, open_path_no_symlinks,
    output_parent,
};

#[expect(
    clippy::items_after_statements,
    clippy::verbose_bit_mask,
    reason = "the Unix permission check is platform-scoped and mirrors the filesystem mode contract"
)]
pub fn directory_tree_bytes(path: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let mut entries = BTreeMap::new();
    for entry in WalkDir::new(path).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(path)?.to_owned();
        use std::os::unix::fs::MetadataExt;
        let metadata = entry.metadata()?;
        ensure!(
            metadata.uid() == effective_uid() && metadata.mode() & 0o077 == 0,
            "published directory entry must be owned by the current user and owner-only: {}",
            entry.path().display()
        );
        let file_type = entry.file_type();
        let bytes = if file_type.is_dir() {
            None
        } else {
            ensure!(
                !file_type.is_symlink() && metadata.is_file(),
                "published directory contains an unsupported entry: {}",
                entry.path().display()
            );
            Some(fs::read(entry.path())?)
        };
        entries.insert(relative, bytes);
    }
    Ok(entries)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

#[expect(
    unsafe_code,
    reason = "atomic no-replace publication requires the Linux renameat2 libc ABI"
)]
pub(super) fn rename_no_replace(source: &Path, output: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source =
        CString::new(source.as_os_str().as_bytes()).context("source contains a NUL byte")?;
    let output =
        CString::new(output.as_os_str().as_bytes()).context("output contains a NUL byte")?;
    // SAFETY: both pointers are valid NUL-terminated path strings for the duration of the call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            output.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "publish {} without replacing an existing output",
                output.to_string_lossy()
            )
        })
    }
}

pub fn rename_directory_no_replace(source: &Path, output: &Path) -> Result<()> {
    rename_no_replace(source, output)
}

pub(super) fn private_parent(path: &Path) -> Result<File> {
    ensure_outside_evidence_package(path)?;
    validate_private_output_parent(path)
}

fn validate_private_output_parent(path: &Path) -> Result<File> {
    use std::os::unix::fs::MetadataExt;
    let parent = output_parent(path);
    let directory = open_path_no_symlinks(parent, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let opened = directory.metadata()?;
    let named_matches = if bound_descriptor_path(parent) {
        true
    } else {
        let named = fs::symlink_metadata(parent)
            .with_context(|| format!("inspect output parent {}", parent.display()))?;
        !named.file_type().is_symlink()
            && named.dev() == opened.dev()
            && named.ino() == opened.ino()
    };
    ensure!(
        opened.is_dir()
            && opened.uid() == effective_uid()
            && opened.mode() & 0o022 == 0
            && named_matches,
        "output parent must be an owner-controlled non-writable directory: {}",
        parent.display()
    );
    Ok(directory)
}
