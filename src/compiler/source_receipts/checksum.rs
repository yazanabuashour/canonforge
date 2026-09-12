use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, ensure};

use crate::protected_fs::read_bound_private_file;

#[expect(
    clippy::arithmetic_side_effects,
    reason = "checksum diagnostics use one-based line numbers over an in-memory file"
)]
pub(in crate::compiler) fn checksum_index(path: &Path) -> Result<HashMap<String, String>> {
    let bytes = read_bound_private_file(path)?.bytes;
    let text = std::str::from_utf8(&bytes).context("checksum index must be UTF-8")?;
    let mut checksums = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let digest = line
            .get(..64)
            .with_context(|| format!("invalid checksum line {}", index + 1))?;
        let remainder = line
            .get(64..)
            .with_context(|| format!("invalid checksum line {}", index + 1))?;
        ensure!(
            remainder.starts_with(' ') || remainder.starts_with('\t'),
            "invalid checksum line {}",
            index + 1
        );
        let name = remainder.trim_start_matches([' ', '\t']);
        let name = name.strip_prefix("./").unwrap_or(name).to_owned();
        ensure!(
            digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !name.is_empty()
                && !name.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
                && checksums
                    .insert(name, digest.to_ascii_lowercase())
                    .is_none(),
            "invalid or duplicate checksum line {}",
            index + 1
        );
    }
    Ok(checksums)
}
