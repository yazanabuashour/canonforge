use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::TargetLock;

mod directory;
mod output;

pub use directory::ensure_private_relative_directory;
pub(super) use directory::{ensure_outside_evidence_package, staging_paths};
pub use output::private_staging_writer;

impl TargetLock {
    pub fn acquire_bound(target: &Path) -> Result<Self> {
        let mut path = target.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let file = Self::open(&path)?;
        file.try_lock()
            .with_context(|| format!("target is locked: {}", path.display()))?;
        Ok(Self { _file: file })
    }

    pub(super) fn acquire_bound_blocking(target: &Path) -> Result<Self> {
        let mut path = target.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let file = Self::open(&path)?;
        file.lock()
            .with_context(|| format!("wait for target lock: {}", path.display()))?;
        Ok(Self { _file: file })
    }

    #[expect(
        clippy::items_after_statements,
        reason = "the Unix permission trait is scoped beside the permission normalization"
    )]
    fn open(path: &Path) -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .with_context(|| format!("open target lock {}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
}
