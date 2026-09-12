use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use super::{SourceFile, VerifiedSource, email_attachments::verify_artifact, json_support::digest};
use crate::protected_fs::{digest_bound_private_file, read_bound_private_file, safe_join};

mod checksum;

pub(super) struct SourceReceipts<'a> {
    root: &'a Path,
    checksums: HashMap<String, String>,
    pending: HashSet<String>,
    expected: HashMap<String, SourceFile>,
    verified: HashMap<String, SourceFile>,
}

impl<'a> SourceReceipts<'a> {
    pub(super) fn new(root: &'a Path, checksums: &Path, planned: HashSet<String>) -> Result<Self> {
        Ok(Self {
            root,
            checksums: checksum::checksum_index(checksums)?,
            pending: planned,
            expected: HashMap::new(),
            verified: HashMap::new(),
        })
    }

    pub(super) fn read(
        &mut self,
        relative: &str,
        read_bytes: bool,
    ) -> Result<(VerifiedSource, (u64, u64))> {
        ensure!(
            self.pending.contains(relative),
            "source was not planned or was read twice: {relative}"
        );
        let (source, identity) = verified_source(
            &safe_join(self.root, relative)?,
            relative,
            read_bytes,
            &self.checksums,
        )?;
        if let Some(expected) = self.expected.get(relative) {
            ensure!(
                expected == &source.receipt,
                "verified source receipt disagrees with email attachment receipt: {relative}"
            );
        }
        self.expected.remove(relative);
        self.pending.remove(relative);
        self.verified
            .insert(relative.to_owned(), source.receipt.clone());
        Ok((source, identity))
    }

    pub(super) fn require_artifact(&mut self, source: &SourceFile) -> Result<()> {
        if let Some(found) = self
            .verified
            .get(&source.path)
            .or_else(|| self.expected.get(&source.path))
        {
            ensure!(
                found == source,
                "email attachment manifests disagree about source receipt {}",
                source.path
            );
        } else if self.pending.contains(&source.path) {
            // The source pass will verify this artifact against both its checksum and this receipt.
            self.expected.insert(source.path.clone(), source.clone());
        } else {
            verify_artifact(self.root, source)?;
            self.verified.insert(source.path.clone(), source.clone());
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<()> {
        ensure!(self.pending.is_empty(), "planned sources remain unread");
        ensure!(
            self.expected.is_empty(),
            "email attachment receipts remain unverified"
        );
        Ok(())
    }
}

fn verified_source(
    path: &Path,
    relative: &str,
    read_bytes: bool,
    checksums: &HashMap<String, String>,
) -> Result<(VerifiedSource, (u64, u64))> {
    #[cfg(test)]
    super::VERIFIED_SOURCE_READS.set(super::VERIFIED_SOURCE_READS.get().saturating_add(1));
    if read_bytes {
        let snapshot = read_bound_private_file(path)?;
        let sha256 = digest(&snapshot.bytes);
        ensure!(
            checksums.get(relative) == Some(&sha256),
            "checksum mismatch or missing checksum for {relative}"
        );
        let bytes = u64::try_from(snapshot.bytes.len()).context("source byte count overflow")?;
        return Ok((
            VerifiedSource {
                receipt: SourceFile {
                    path: relative.into(),
                    sha256,
                    bytes,
                },
                bytes: snapshot.bytes,
            },
            (snapshot.device, snapshot.inode),
        ));
    }
    let snapshot = digest_bound_private_file(path)?;
    ensure!(
        checksums.get(relative) == Some(&snapshot.sha256),
        "checksum mismatch or missing checksum for {relative}"
    );
    Ok((
        VerifiedSource {
            receipt: SourceFile {
                path: relative.into(),
                sha256: snapshot.sha256,
                bytes: snapshot.bytes,
            },
            bytes: Vec::new(),
        },
        (snapshot.device, snapshot.inode),
    ))
}
