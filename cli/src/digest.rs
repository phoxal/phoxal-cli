//! Content digests over local files.
//!
//! One helper, used by both places this client hashes a file it did not
//! produce: verifying a downloaded release archive against its published
//! checksum, and naming an installed release after the archive it came from.
//! Both want the same answer in the same form, so they ask the same function.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Streaming read: an archive is arbitrarily large and is never worth holding
/// in memory just to hash it.
const CHUNK_BYTES: usize = 64 * 1024;

/// The lowercase hex SHA256 of the file at `path`.
///
/// # Errors
///
/// When the file cannot be opened or read.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty-input digest is the one SHA256 value that can be checked
    /// against the published constant rather than against ourselves.
    #[test]
    fn an_empty_file_hashes_to_the_published_sha256_of_nothing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("empty");
        fs::write(&path, b"")?;
        assert_eq!(
            sha256_file(&path)?,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        Ok(())
    }

    /// Larger than one read chunk, so the streaming loop is actually exercised
    /// rather than only its first pass.
    #[test]
    fn a_file_larger_than_one_chunk_is_hashed_across_reads() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("big");
        let contents = vec![7_u8; CHUNK_BYTES * 2 + 13];
        fs::write(&path, &contents)?;

        let mut expected = Sha256::new();
        expected.update(&contents);
        assert_eq!(sha256_file(&path)?, hex::encode(expected.finalize()));
        Ok(())
    }
}
