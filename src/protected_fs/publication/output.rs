use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use super::{
    super::{
        BoundOutput, GuardedPublicWriter, PrivateWriter, open_path_no_symlinks, output_parent,
        private_mode, private_parent, sync_directory,
    },
    directory::ensure_no_reserved_output_components,
};

impl BoundOutput {
    pub fn open(output: &Path) -> Result<Self> {
        ensure_no_reserved_output_components(output)?;
        let destination = Self::open_internal(output)?;
        ensure_no_reserved_output_components(destination.resolved())?;
        Ok(destination)
    }

    // Only internal writes inside already-bound staging or artifact directories may bypass
    // caller-selected public output checks.
    pub(in crate::protected_fs) fn open_internal(output: &Path) -> Result<Self> {
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

    pub fn into_guarded_public_writer(self) -> Result<GuardedPublicWriter> {
        Ok(GuardedPublicWriter(PrivateWriter::new(self)?))
    }

    pub(super) fn verified_parent(&self) -> Result<File> {
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

impl Write for PrivateWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl PrivateWriter {
    fn new(destination: BoundOutput) -> Result<Self> {
        let temporary = NamedTempFile::new_in(destination.parent()).with_context(|| {
            format!(
                "create temporary output for {}",
                destination.resolved().display()
            )
        })?;
        Ok(Self {
            inner: BufWriter::new(temporary),
            destination,
        })
    }

    fn prepare(mut self) -> Result<(BoundOutput, NamedTempFile)> {
        self.inner.flush()?;
        let temporary = self
            .inner
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        private_mode(temporary.path())?;
        temporary.as_file().sync_all()?;
        Ok((self.destination, temporary))
    }

    pub fn finish(self) -> Result<()> {
        let (destination, temporary) = self.prepare()?;
        temporary
            .persist(destination.target())
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", destination.resolved().display()))?;
        sync_directory(destination.parent())?;
        Ok(())
    }
}

impl Write for GuardedPublicWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl GuardedPublicWriter {
    pub fn finish(self) -> Result<()> {
        let (destination, temporary) = self.0.prepare()?;
        let name = destination
            .target()
            .file_name()
            .context("bound output must have a filename")?;
        let publication_parent = destination.verified_parent()?;
        temporary
            .persist(
                Path::new("/proc/self/fd")
                    .join(publication_parent.as_raw_fd().to_string())
                    .join(name),
            )
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", destination.resolved().display()))?;
        publication_parent.sync_all().with_context(|| {
            format!(
                "sync output parent {}",
                destination.requested_parent.display()
            )
        })?;
        Ok(())
    }
}

pub fn private_staging_writer(path: &Path) -> Result<PrivateWriter> {
    PrivateWriter::new(BoundOutput::open_internal(path)?)
}
