use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

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

impl BoundOutput {
    pub fn open(output: &Path) -> Result<Self> {
        ensure_public_output_path(output)?;
        Self::open_internal(output)
    }

    fn open_internal(output: &Path) -> Result<Self> {
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

    fn acquire_bound_blocking(target: &Path) -> Result<Self> {
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
            let _ = sync_directory(&self.parent);
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

fn staging_paths(target: &Path) -> (PathBuf, PathBuf) {
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

fn ensure_outside_evidence_package(output: &Path) -> Result<()> {
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
        let bytes = if entry.file_type().is_dir() {
            None
        } else {
            ensure!(
                entry.file_type().is_file(),
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

fn rename_no_replace(source: &Path, output: &Path) -> Result<()> {
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

fn private_parent(path: &Path) -> Result<File> {
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

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and no preconditions.
    unsafe { libc::geteuid() }
}

fn bound_descriptor_path(path: &Path) -> bool {
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

fn output_parent(path: &Path) -> &Path {
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

fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
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

fn bind_private_parent(path: &Path) -> Result<(File, PathBuf)> {
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
fn validate_private_metadata(metadata: &fs::Metadata, path: &Path, directory: bool) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn current_directory_can_be_bound_as_private_input() {
        bind_private_parent(Path::new(".")).unwrap();
    }

    #[test]
    fn concurrent_identical_blob_publication_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blob");
        let bytes = b"fictional shared attachment";
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let other_barrier = Arc::clone(&barrier);
            let other_path = path.clone();
            let other_digest = sha256.clone();
            let handle = scope.spawn(move || {
                other_barrier.wait();
                publish_content_addressed_blob(&other_path, &other_digest, bytes)
            });
            barrier.wait();
            publish_content_addressed_blob(&path, &sha256, bytes).unwrap();
            handle.join().unwrap().unwrap();
        });
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!PathBuf::from(format!("{}.staging", path.display())).exists());
    }
}
