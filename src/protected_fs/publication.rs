use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::{
    BoundOutput, BoundPrivateDirectory, PrivateDirectory, PrivateWriter, TargetLock,
    directory_tree_bytes, open_path_no_symlinks, open_private_bound_directory, output_parent,
    private_mode, private_parent, read_bound_private_json, rename_directory_no_replace,
    resolve_existing_ancestor, sync_directory, validate_private_metadata,
};

impl BoundOutput {
    pub fn open(output: &Path) -> Result<Self> {
        ensure_public_output_path(output)?;
        Self::open_internal(output)
    }

    pub(super) fn open_internal(output: &Path) -> Result<Self> {
        let parent = private_parent(output)?;
        let name = output
            .file_name()
            .context("output must name one file or directory")?;
        let bound_parent = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let resolved_parent = fs::canonicalize(&bound_parent)?;
        let resolved = resolved_parent.join(name);
        let target = bound_parent.join(name);
        Ok(Self {
            parent_handle: parent,
            requested_parent: output_parent(output).to_owned(),
            parent: bound_parent,
            target,
            resolved,
        })
    }

    pub fn parent(&self) -> &Path {
        &self.parent
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn resolved(&self) -> &Path {
        &self.resolved
    }

    fn verified_parent(&self) -> Result<File> {
        use std::os::unix::fs::MetadataExt;
        let reopened =
            open_path_no_symlinks(&self.requested_parent, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let expected = self.parent_handle.metadata()?;
        let actual = reopened.metadata()?;
        ensure!(
            expected.dev() == actual.dev() && expected.ino() == actual.ino(),
            "output parent changed before publication: {}",
            self.requested_parent.display()
        );
        Ok(reopened)
    }
}

impl PrivateDirectory {
    #[expect(
        clippy::items_after_statements,
        reason = "the Unix directory extension trait is scoped beside private staging creation"
    )]
    pub fn new(output: &Path) -> Result<Self> {
        let target = BoundOutput::open(output)?;
        let lock = TargetLock::acquire_bound(target.target())?;
        let (staging, owner) = prepare_target_staging(&target)?;
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&staging)?;
        sync_directory(target.parent())?;
        Ok(Self {
            staging,
            owner,
            target,
            _lock: lock,
            finished: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.staging
    }

    pub fn finish(mut self) -> Result<()> {
        persist_directory(self.path(), &self.target)?;
        self.finished = true;
        // Publication is committed. A retained marker is recovered by the next
        // locked attempt, so cleanup must not turn success into a false failure.
        if fs::remove_file(&self.owner).is_ok() {
            let _ = sync_directory(self.target.parent());
        }
        Ok(())
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if !self.finished {
            let staging_removed = match fs::remove_dir_all(&self.staging) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            if staging_removed && sync_directory(self.target.parent()).is_ok() {
                let owner_removed = match fs::remove_file(&self.owner) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(_) => false,
                };
                if owner_removed {
                    let _ = sync_directory(self.target.parent());
                }
            }
        }
    }
}

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

impl Write for PrivateWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl PrivateWriter {
    pub fn finish(mut self) -> Result<()> {
        self.inner.flush()?;
        let temporary = self
            .inner
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        private_mode(temporary.path())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.destination.target())
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", self.destination.resolved().display()))?;
        sync_directory(self.destination.parent())?;
        Ok(())
    }
}

pub fn private_staging_writer(path: &Path) -> Result<PrivateWriter> {
    let destination = BoundOutput::open_internal(path)?;
    let temporary = NamedTempFile::new_in(destination.parent())
        .with_context(|| format!("create temporary output for {}", path.display()))?;
    Ok(PrivateWriter {
        inner: BufWriter::new(temporary),
        destination,
    })
}

pub fn ensure_private_relative_directory(
    root: &BoundPrivateDirectory,
    relative: &Path,
) -> Result<()> {
    use std::{os::unix::fs::DirBuilderExt, path::Component};

    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "unsafe relative directory: {}",
        relative.display()
    );
    let mut current = root.path().to_owned();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(()) => sync_directory(
                current
                    .parent()
                    .context("created private directory has no parent")?,
            )?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create private directory {}", current.display()));
            }
        }
        let directory = open_path_no_symlinks(&current, libc::O_RDONLY | libc::O_DIRECTORY)?;
        validate_private_metadata(&directory.metadata()?, &current, true)?;
    }
    Ok(())
}

pub(super) fn staging_paths(target: &Path) -> (PathBuf, PathBuf) {
    let mut staging = target.as_os_str().to_owned();
    staging.push(".staging");
    let staging = PathBuf::from(staging);
    let mut owner = staging.as_os_str().to_owned();
    owner.push(".owner.json");
    let owner = PathBuf::from(owner);
    (staging, owner)
}

fn prepare_target_staging(target: &BoundOutput) -> Result<(PathBuf, PathBuf)> {
    let (staging, owner) = staging_paths(target.target());
    let marker = format!(
        "{:x}",
        Sha256::digest(target.resolved().as_os_str().as_encoded_bytes())
    );
    let marker_matches = match fs::symlink_metadata(&owner) {
        Ok(_) => {
            let found: String = read_bound_private_json(&owner)?;
            ensure!(
                found == marker,
                "staging owner does not match output: {}",
                owner.display()
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect staging owner {}", owner.display()));
        }
    };
    match fs::symlink_metadata(&staging) {
        Ok(metadata) if marker_matches => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "interrupted staging is not a directory: {}",
                staging.display()
            );
            let bound = open_private_bound_directory(&staging)?;
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove interrupted staging {}", staging.display()))?;
            drop(bound);
            sync_directory(target.parent())?;
        }
        Ok(_) => bail!("refuse to remove unowned staging: {}", staging.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect staging {}", staging.display()));
        }
    }
    if !marker_matches {
        let mut file = NamedTempFile::new_in(target.parent())?;
        serde_json::to_writer(&mut file, &marker)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&owner)
            .map_err(|error| error.error)
            .with_context(|| format!("publish staging owner {}", owner.display()))?;
        sync_directory(target.parent())?;
    }
    Ok((staging, owner))
}

fn is_published_evidence_package(path: &Path) -> Result<bool> {
    fn matches(path: &Path, predicate: impl FnOnce(&fs::Metadata) -> bool) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(predicate(&metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        }
    }

    Ok(matches(&path.join("manifest.json"), fs::Metadata::is_file)?
        && matches(&path.join("units"), fs::Metadata::is_dir)?)
}

pub(super) fn ensure_outside_evidence_package(output: &Path) -> Result<()> {
    let resolved_parent = resolve_existing_ancestor(output_parent(output))?;
    for ancestor in resolved_parent.ancestors() {
        ensure!(
            !is_published_evidence_package(ancestor)?,
            "refuse to publish inside an immutable evidence package: {}",
            ancestor.display()
        );
    }
    Ok(())
}

fn ensure_public_output_path(output: &Path) -> Result<()> {
    for component in output.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        let name = name.as_encoded_bytes();
        ensure!(
            ![&b".lock"[..], &b".staging"[..], &b".staging.owner.json"[..],]
                .iter()
                .any(|suffix| name.ends_with(suffix)),
            "output path uses a reserved lock or staging component: {}",
            output.display()
        );
    }
    Ok(())
}

fn persist_directory(staging: &Path, target: &BoundOutput) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700))?;
    sync_directory(staging)?;
    let publication_parent_file = target.verified_parent()?;
    let publication_parent = PathBuf::from(format!(
        "/proc/self/fd/{}",
        publication_parent_file.as_raw_fd()
    ));
    let name = target
        .target()
        .file_name()
        .context("bound output must have a filename")?;
    let publication_target = publication_parent.join(name);
    match fs::symlink_metadata(&publication_target) {
        Ok(_) => {
            let bound_output = open_private_bound_directory(&publication_target)?;
            ensure!(
                directory_tree_bytes(staging)? == directory_tree_bytes(bound_output.path())?,
                "output already exists with different contents: {}",
                target.resolved().display()
            );
            sync_directory(bound_output.path())?;
            fs::remove_dir_all(staging).with_context(|| {
                format!(
                    "remove identical output staging for {}",
                    target.resolved().display()
                )
            })?;
            sync_directory(&publication_parent)?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect output directory {}", target.resolved().display())
            });
        }
    }
    rename_directory_no_replace(staging, &publication_target)?;
    sync_directory(&publication_parent)?;
    Ok(())
}
