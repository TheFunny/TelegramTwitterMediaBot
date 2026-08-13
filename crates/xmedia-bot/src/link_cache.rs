//! Persistent cache of successfully sent posts.
//!
//! After a media send succeeds, the raw render data plus the Telegram
//! `file_id`s of the sent items are stored keyed by [`crate::site` cache
//! key]. A repeated link is then answered entirely from local state — no
//! re-fetch of the source site, no re-upload — and no media file is stored
//! on disk (the file ids point at Telegram's servers). Entries expire after
//! [`Config::link_cache_ttl`]; a stale entry is dropped lazily on read and
//! by the periodic prune in `main`.

use crate::db::now_f64;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CachedMediaKind {
    Photo,
    Video,
    Animation,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CachedMedia {
    pub kind: CachedMediaKind,
    pub file_id: String,
}

/// Everything needed to re-send a post without touching the source site:
/// the canonical URL, pre-escaped caption fields, and the file ids produced
/// by the original successful send.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CachedPost {
    pub url: String,
    /// The site's built-in caption (used when the chat has no format
    /// override).
    pub caption: String,
    pub title: String,
    pub author: String,
    pub author_url: String,
    pub tags: String,
    pub sensitive: bool,
    pub media: Vec<CachedMedia>,
}

/// SQLite-backed cache sharing `data/task_queue.db` with the queue and chat
/// state (same `open_db` pattern: busy timeout, `spawn_blocking` I/O).
pub struct LinkCache {
    pool: crate::db::DbPool,
}

impl LinkCache {
    pub fn open(db_path: &str) -> Self {
        if let Ok(conn) = Connection::open(db_path)
            && let Err(e) = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS link_cache (url TEXT PRIMARY KEY, \
                 payload TEXT NOT NULL, created_at REAL NOT NULL);",
            )
        {
            log::error!("failed to initialize link cache schema: {e}");
        }
        Self {
            pool: crate::db::DbPool::new(db_path),
        }
    }

    /// Returns the cached post if present and not expired; a stale entry is
    /// removed on the spot.
    pub async fn get(&self, key: &str, ttl: Duration) -> Option<CachedPost> {
        let key = key.to_string();
        let ttl = ttl.as_secs_f64();
        let result = self
            .pool
            .with_conn(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT payload, created_at FROM link_cache WHERE url = ?1")?;
                let mut rows = stmt.query(params![key])?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };
                let payload: String = row.get(0)?;
                let created_at: f64 = row.get(1)?;
                if now_f64() - created_at > ttl {
                    conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])?;
                    return Ok(None);
                }
                Ok(Some(serde_json::from_str::<CachedPost>(&payload).map_err(
                    |e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
                )?))
            })
            .await;
        match result {
            Ok(v) => v,
            Err(e) => {
                log::error!("link cache read failed: {e}");
                None
            }
        }
    }

    pub async fn put(&self, key: &str, post: &CachedPost) {
        let key = key.to_string();
        let payload = serde_json::to_string(post).expect("cached post serializes");
        let result = self
            .pool
            .with_conn(move |conn| {
                conn.execute(
                "INSERT OR REPLACE INTO link_cache (url, payload, created_at) VALUES (?1, ?2, ?3)",
                params![key, payload, now_f64()],
            )?;
                Ok(())
            })
            .await;
        if let Err(e) = result {
            log::error!("link cache write failed: {e}");
        }
    }

    /// Drops an entry (e.g. a cached file id that turned out invalid).
    pub async fn remove(&self, key: &str) {
        let key = key.to_string();
        let result = self
            .pool
            .with_conn(move |conn| {
                conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])?;
                Ok(())
            })
            .await;
        if let Err(e) = result {
            log::error!("link cache delete failed: {e}");
        }
    }

    /// Removes expired entries; returns how many were deleted.
    pub async fn prune(&self, ttl: Duration) -> usize {
        let cutoff = now_f64() - ttl.as_secs_f64();
        let result = self
            .pool
            .with_conn(move |conn| {
                conn.execute(
                    "DELETE FROM link_cache WHERE created_at < ?1",
                    params![cutoff],
                )
            })
            .await;
        match result {
            Ok(n) => n,
            Err(e) => {
                log::error!("link cache prune failed: {e}");
                0
            }
        }
    }

    /// Deletes one entry (by normalized cache key) or the whole cache when
    /// `key` is `None`. Returns how many rows were removed.
    pub async fn clear(&self, key: Option<&str>) -> usize {
        let key = key.map(str::to_string);
        let result = self
            .pool
            .with_conn(move |conn| match &key {
                Some(key) => conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key]),
                None => conn.execute("DELETE FROM link_cache", []),
            })
            .await;
        match result {
            Ok(n) => n,
            Err(e) => {
                log::error!("link cache clear failed: {e}");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> CachedPost {
        CachedPost {
            url: "https://x.com/u/status/1".into(),
            caption: "cap".into(),
            title: "t".into(),
            author: "a".into(),
            author_url: "au".into(),
            tags: "".into(),
            sensitive: true,
            media: vec![CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: "AgAC...".into(),
            }],
        }
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::open(dir.path().join("c.db").to_str().unwrap());
        cache.put("twitter:1", &entry()).await;
        let got = cache.get("twitter:1", Duration::from_secs(3600)).await;
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.url, "https://x.com/u/status/1");
        assert_eq!(got.media[0].file_id, "AgAC...");
    }

    #[tokio::test]
    async fn expired_entry_removed_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::open(dir.path().join("c.db").to_str().unwrap());
        cache.put("twitter:1", &entry()).await;
        // Force the row into the past so a 1s TTL expires it.
        {
            let conn = Connection::open(dir.path().join("c.db")).unwrap();
            conn.execute("UPDATE link_cache SET created_at = created_at - 100", [])
                .unwrap();
        }
        assert!(
            cache
                .get("twitter:1", Duration::from_secs(1))
                .await
                .is_none()
        );
        assert!(
            cache
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn remove_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::open(dir.path().join("c.db").to_str().unwrap());
        cache.put("twitter:1", &entry()).await;
        cache.put("pixiv:2", &entry()).await;
        cache.remove("twitter:1").await;
        assert!(
            cache
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none()
        );
        assert!(
            cache
                .get("pixiv:2", Duration::from_secs(3600))
                .await
                .is_some()
        );
        {
            let conn = Connection::open(dir.path().join("c.db")).unwrap();
            conn.execute("UPDATE link_cache SET created_at = created_at - 100", [])
                .unwrap();
        }
        assert_eq!(cache.prune(Duration::from_secs(1)).await, 1);
        assert!(
            cache
                .get("pixiv:2", Duration::from_secs(3600))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn clear_one_entry_or_all() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::open(dir.path().join("c.db").to_str().unwrap());
        cache.put("twitter:1", &entry()).await;
        cache.put("pixiv:2", &entry()).await;
        // By key: only the matching row is removed.
        assert_eq!(cache.clear(Some("twitter:1")).await, 1);
        assert!(
            cache
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none()
        );
        assert!(
            cache
                .get("pixiv:2", Duration::from_secs(3600))
                .await
                .is_some()
        );
        // Whole cache: nothing left; removing an absent key deletes 0 rows.
        assert_eq!(cache.clear(None).await, 1);
        assert!(
            cache
                .get("pixiv:2", Duration::from_secs(3600))
                .await
                .is_none()
        );
        assert_eq!(cache.clear(None).await, 0);
    }
}
