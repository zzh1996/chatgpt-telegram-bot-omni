//! Content-addressed blob storage on disk: `<data>/blobs/ab/cd/<sha256>`.
//! Writes are atomic (temp file + rename) and idempotent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use omni_core::BlobRef;
use rusqlite::params;

use super::{Db, now_ms};

#[derive(Clone)]
pub struct BlobStore {
    root: PathBuf,
    db: Db,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>, db: Db) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        Ok(Self { root, db })
    }

    pub fn path_of(&self, sha: &str) -> PathBuf {
        self.root.join(omni_core::blob::rel_path_for(sha))
    }

    /// Store bytes synchronously (safe to call from blocking contexts and loggers).
    pub fn put_sync(&self, bytes: &[u8], mime: &str) -> Result<BlobRef> {
        let r = BlobRef::for_bytes(bytes, mime);
        let path = self.path_of(&r.sha256);
        if !path.exists() {
            write_atomic(&path, bytes)?;
        }
        let (sha, size, mime) = (r.sha256.clone(), r.size as i64, r.mime.clone());
        self.db.call_sync(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO blobs(sha256, size, mime, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![sha, size, mime, now_ms()],
            )?;
            Ok(())
        })?;
        Ok(r)
    }

    pub async fn put(&self, bytes: Vec<u8>, mime: String) -> Result<BlobRef> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.put_sync(&bytes, &mime)).await.context("blob task panicked")?
    }

    pub async fn get(&self, r: &BlobRef) -> Result<Vec<u8>> {
        let path = self.path_of(&r.sha256);
        let bytes = tokio::fs::read(&path).await.with_context(|| format!("read blob {}", path.display()))?;
        Ok(bytes)
    }

    pub fn exists(&self, sha: &str) -> bool {
        self.path_of(sha).exists()
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("blob path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), rand::random::<u64>()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let store = BlobStore::new(dir.path().join("blobs"), db).unwrap();
        let a = store.put(b"abc".to_vec(), "text/plain".into()).await.unwrap();
        let b = store.put(b"abc".to_vec(), "text/plain".into()).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(store.get(&a).await.unwrap(), b"abc");
        assert!(store.exists(&a.sha256));
    }
}
