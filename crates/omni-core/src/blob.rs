//! Content-addressed blob references. Blob bytes live on disk
//! (`data/blobs/ab/cd/<sha256>`); the database and logs only keep references.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    pub size: u64,
    /// MIME type as known when the blob was created (e.g. from Telegram or the provider).
    pub mime: String,
}

impl BlobRef {
    pub fn for_bytes(bytes: &[u8], mime: impl Into<String>) -> Self {
        Self { sha256: sha256_hex(bytes), size: bytes.len() as u64, mime: mime.into() }
    }

    /// Relative storage path: `ab/cd/abcdef...`; `None` if the name is not a
    /// SHA-256 (it would not stay inside the store).
    pub fn rel_path(&self) -> Option<String> {
        rel_path_for(&self.sha256)
    }

    pub fn short(&self) -> &str {
        &self.sha256[..self.sha256.len().min(12)]
    }
}

impl fmt::Display for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<blob sha256:{} {} bytes {}>", self.short(), self.size, self.mime)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `s` is a blob name: a lowercase hex SHA-256. Blob references come
/// from stored data that providers and models partly control, so nothing else
/// may become a path.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The storage path of a blob name (`ab/cd/abcdef...`), or `None` for anything
/// that is not a lowercase hex SHA-256 (`../x`, absolute paths, other text).
pub fn rel_path_for(sha: &str) -> Option<String> {
    is_sha256_hex(sha).then(|| format!("{}/{}/{}", &sha[0..2], &sha[2..4], sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_layout() {
        let b = BlobRef::for_bytes(b"hello", "text/plain");
        assert_eq!(b.sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(
            b.rel_path().as_deref(),
            Some("2c/f2/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        // Only a hex SHA-256 is a blob name: nothing else becomes a path.
        let dots = format!("../{}", "./".repeat(30)) + "x";
        assert_eq!(dots.len(), 64);
        for bad in [dots.as_str(), &"A".repeat(64), &"0".repeat(63), &"é".repeat(32), ""] {
            assert_eq!(rel_path_for(bad), None, "{bad}");
        }
        assert_eq!(b.to_string(), "<blob sha256:2cf24dba5fb0 5 bytes text/plain>");
    }
}
