use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use super::{
    BoundOutput, TargetLock, digest_bound_private_file, private_mode, rename_no_replace,
    sync_directory, validate_private_metadata,
};

struct BlobStaging {
    path: Option<PathBuf>,
    parent: PathBuf,
}

impl BlobStaging {
    const fn new(path: PathBuf, parent: PathBuf) -> Self {
        Self {
            path: Some(path),
            parent,
        }
    }

    fn finish(&mut self) {
        self.path = None;
    }
}

impl Drop for BlobStaging {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && fs::remove_file(path).is_ok()
        {
            drop(sync_directory(&self.parent));
        }
    }
}

pub fn publish_content_addressed_blob(
    path: &Path,
    expected_sha256: &str,
    bytes: &[u8],
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let actual_sha256 = format!("{:x}", Sha256::digest(bytes));
    ensure!(
        expected_sha256.len() == 64
            && expected_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            && actual_sha256 == expected_sha256,
        "content-addressed blob digest mismatch: expected {expected_sha256}, actual {actual_sha256}"
    );
    let destination = BoundOutput::open_internal(path)?;
    let _lock = TargetLock::acquire_bound_blocking(destination.target())?;
    let mut staging = destination.target().as_os_str().to_owned();
    staging.push(".staging");
    let staging = PathBuf::from(staging);
    match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            validate_private_metadata(&metadata, &staging, false)?;
            fs::remove_file(&staging).with_context(|| {
                format!("remove interrupted blob staging {}", staging.display())
            })?;
            sync_directory(destination.parent())?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect blob staging {}", staging.display()));
        }
    }
    match fs::symlink_metadata(destination.target()) {
        Ok(_) => {
            let found = digest_bound_private_file(destination.target())?;
            let requested_bytes =
                u64::try_from(bytes.len()).context("artifact byte count overflow")?;
            ensure!(
                found.sha256 == expected_sha256 && found.bytes == requested_bytes,
                "existing artifact {} contains different bytes: expected sha256 {expected_sha256} and {requested_bytes} bytes, found sha256 {} and {} bytes",
                destination.resolved().display(),
                found.sha256,
                found.bytes
            );
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect existing artifact {}",
                    destination.resolved().display()
                )
            });
        }
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&staging)
        .with_context(|| format!("create blob staging {}", staging.display()))?;
    let mut cleanup = BlobStaging::new(staging.clone(), destination.parent().to_owned());
    file.write_all(bytes)?;
    file.sync_all()?;
    private_mode(&staging)?;
    rename_no_replace(&staging, destination.target())?;
    cleanup.finish();
    sync_directory(destination.parent())
}
