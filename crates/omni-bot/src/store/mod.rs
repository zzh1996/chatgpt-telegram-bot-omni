//! SQLite persistence.
//!
//! A single connection in WAL mode guarded by a mutex; all access goes through
//! [`Db::call`], which runs the closure on Tokio's blocking pool. SQLite is
//! single-writer anyway and our write rate is low.

pub mod blobs;
pub mod schema;
pub mod session;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.migrate_blocking()?;
        Ok(db)
    }

    fn migrate_blocking(&self) -> Result<()> {
        let mut conn = self.conn.lock();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, m) in schema::MIGRATIONS.iter().enumerate() {
            let v = i as i64 + 1;
            if v <= version {
                continue;
            }
            let tx = conn.transaction()?;
            tx.execute_batch(m).with_context(|| format!("migration v{v}"))?;
            tx.pragma_update(None, "user_version", v)?;
            tx.commit()?;
            tracing::info!(version = v, "applied database migration");
        }
        Ok(())
    }

    /// Run `f` with the connection on the blocking pool.
    pub async fn call<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock();
            f(&mut c)
        })
        .await
        .context("database task panicked")?
    }

    /// Synchronous access (for code that is already on a blocking thread, and tests).
    pub fn call_sync<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut c = self.conn.lock();
        f(&mut c)
    }

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.call(move |c| {
            Ok(c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
        })
        .await
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_string(), value.to_string());
        self.call(move |c| {
            c.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_and_meta_works() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta_get("x").await.unwrap(), None);
        db.meta_set("x", "1").await.unwrap();
        db.meta_set("x", "2").await.unwrap();
        assert_eq!(db.meta_get("x").await.unwrap().as_deref(), Some("2"));
        let v: i64 = db.call_sync(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?)).unwrap();
        assert_eq!(v as usize, schema::MIGRATIONS.len());
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db.sqlite");
        drop(Db::open(&p).unwrap());
        drop(Db::open(&p).unwrap());
    }
}
