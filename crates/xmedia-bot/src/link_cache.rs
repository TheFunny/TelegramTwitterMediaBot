//! Persistent cache of successfully sent posts.
//!
//! After a media send succeeds, the raw render data plus the Telegram
//! `file_id`s of the sent items are stored keyed by [`crate::site` cache
//! key]. A repeated link is then answered entirely from local state — no
//! re-fetch of the source site, no re-upload — and no media file is stored
//! on disk (the file ids point at Telegram's servers). Entries expire after
//! `Config::link_cache_ttl`; a stale entry is dropped lazily on read and
//! by the periodic prune in `main`.

use crate::db::now_f64;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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
    /// The media URL the send used, kept so an entry whose file ids stopped
    /// working can still be re-sent without touching the source site (see the
    /// bot's `invalidate_cache`). Empty for entries written before this field
    /// existed — those can only be dropped and re-fetched.
    #[serde(default)]
    pub url: String,
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
    /// The post's body text. Defaulted on read: entries written before the
    /// title/content split carry it inside `title`.
    #[serde(default)]
    pub content: String,
    pub author: String,
    pub author_url: String,
    pub tags: String,
    pub sensitive: bool,
    pub media: Vec<CachedMedia>,
}

/// SQLite-backed cache sharing `data/task_queue.db` with the queue and chat
/// state (same shared pool, see [`crate::db::open_store`]).
pub struct LinkCache {
    pool: Arc<crate::db::DbPool>,
}

impl LinkCache {
    /// Wraps the shared DB pool (the `link_cache` table lives in the merged
    /// schema alongside `tasks` and `chat_state`).
    pub fn new(pool: Arc<crate::db::DbPool>) -> Self {
        LinkCache { pool }
    }

    /// Returns the cached post if present and not expired; a stale entry is
    /// removed on the spot.
    pub async fn get(&self, key: &str, ttl: Duration) -> Option<CachedPost> {
        let key = key.to_string();
        let ttl = ttl.as_secs_f64();
        self.pool
            .with_conn_or(
                log::Level::Warn,
                "link cache read failed",
                None,
                move |conn| {
                    let Some((payload, created_at)) = conn
                        .query_row(
                            "SELECT payload, created_at FROM link_cache WHERE url = ?1",
                            params![key],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
                        )
                        .optional()?
                    else {
                        return Ok(None);
                    };
                    if now_f64() - created_at > ttl {
                        conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])?;
                        return Ok(None);
                    }
                    match serde_json::from_str::<CachedPost>(&payload) {
                        Ok(post) => Ok(Some(post)),
                        Err(e) => {
                            // Unreadable payload (e.g. an older schema): drop it
                            // instead of re-failing the parse on every later hit.
                            conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])?;
                            Err(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                        }
                    }
                },
            )
            .await
    }

    pub async fn put(&self, key: &str, post: &CachedPost) {
        let key = key.to_string();
        let payload = serde_json::to_string(post).expect("cached post serializes");
        self.pool
            .with_conn_or(
                log::Level::Warn,
                "link cache write failed",
                (),
                move |conn| {
                    conn.execute(
                        "INSERT OR REPLACE INTO link_cache (url, payload, created_at) VALUES (?1, ?2, ?3)",
                        params![key, payload, now_f64()],
                    )?;
                    Ok(())
                },
            )
            .await;
    }

    /// Drops an entry (e.g. a cached file id that turned out invalid).
    pub async fn remove(&self, key: &str) {
        let key = key.to_string();
        self.pool
            .with_conn_or(
                log::Level::Warn,
                "link cache delete failed",
                (),
                move |conn| {
                    conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])?;
                    Ok(())
                },
            )
            .await;
    }

    /// Removes expired entries; returns how many were deleted.
    pub async fn prune(&self, ttl: Duration) -> usize {
        let cutoff = now_f64() - ttl.as_secs_f64();
        self.pool
            .with_conn_or(
                log::Level::Warn,
                "link cache prune failed",
                0,
                move |conn| {
                    conn.execute(
                        "DELETE FROM link_cache WHERE created_at < ?1",
                        params![cutoff],
                    )
                },
            )
            .await
    }

    /// Deletes one entry (by normalized cache key) or the whole cache when
    /// `key` is `None`. Returns how many rows were removed.
    pub async fn clear(&self, key: Option<&str>) -> usize {
        let key = key.map(str::to_string);
        self.pool
            .with_conn_or(
                log::Level::Warn,
                "link cache clear failed",
                0,
                move |conn| match &key {
                    Some(key) => {
                        conn.execute("DELETE FROM link_cache WHERE url = ?1", params![key])
                    }
                    None => conn.execute("DELETE FROM link_cache", []),
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::cached_photo;

    /// A payload written before the title/content split has no `content`
    /// field. It must still read back — the cache deletes what it cannot
    /// parse — with its text left where it was stored (`title`) and the
    /// caption it replays untouched. No migration: a self-hosted cache entry
    /// lives one TTL, and moving the text would only reshuffle `/set_format`
    /// placeholders until it expires.
    #[tokio::test]
    async fn pre_split_entry_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::new(
            crate::db::open_store(dir.path().join("c.db").to_str().unwrap()).unwrap(),
        );
        let legacy = serde_json::json!({
            "url": "https://x.com/u/status/1",
            "caption": "https://x.com/u/status/1\n<a href=\"au\">a</a>: old text",
            "title": "old text",
            "author": "a",
            "author_url": "au",
            "tags": "",
            "sensitive": false,
            "media": [{"kind": "photo", "file_id": "AgAC..."}]
        });
        {
            let conn = rusqlite::Connection::open(dir.path().join("c.db")).unwrap();
            conn.execute(
                "INSERT INTO link_cache (url, payload, created_at) VALUES (?1, ?2, ?3)",
                params!["twitter:1", legacy.to_string(), now_f64()],
            )
            .unwrap();
        }

        let got = cache
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .expect("a pre-split payload must not be dropped");
        assert_eq!(got.title, "old text");
        assert_eq!(got.content, "");
        assert_eq!(
            got.caption,
            "https://x.com/u/status/1\n<a href=\"au\">a</a>: old text"
        );
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::new(
            crate::db::open_store(dir.path().join("c.db").to_str().unwrap()).unwrap(),
        );
        cache.put("twitter:1", &cached_photo()).await;
        let got = cache.get("twitter:1", Duration::from_secs(3600)).await;
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.url, "https://x.com/u/status/1");
        assert_eq!(got.media[0].file_id, "AgAC-file-id");
        // The source URL rides along: it is what a degraded entry falls back to.
        assert_eq!(got.media[0].url, "https://pbs.twimg.com/media/photo.jpg");
    }

    #[tokio::test]
    async fn expired_entry_removed_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::new(
            crate::db::open_store(dir.path().join("c.db").to_str().unwrap()).unwrap(),
        );
        cache.put("twitter:1", &cached_photo()).await;
        // Force the row into the past so a 1s TTL expires it.
        {
            let conn = rusqlite::Connection::open(dir.path().join("c.db")).unwrap();
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
    async fn unreadable_entry_is_dropped_on_read() {
        // A payload from an older schema must not be re-parsed on every hit:
        // the row is removed and the read reports a miss.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("c.db");
        let cache = LinkCache::new(crate::db::open_store(db_path.to_str().unwrap()).unwrap());
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO link_cache (url, payload, created_at) VALUES (?1, ?2, ?3)",
                params!["twitter:1", "{not json", now_f64()],
            )
            .unwrap();
        }

        assert!(
            cache
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none()
        );
        // Dropped, not left behind for the next hit.
        assert_eq!(cache.clear(None).await, 0, "corrupted row still present");
    }

    #[tokio::test]
    async fn remove_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LinkCache::new(
            crate::db::open_store(dir.path().join("c.db").to_str().unwrap()).unwrap(),
        );
        cache.put("twitter:1", &cached_photo()).await;
        cache.put("pixiv:2", &cached_photo()).await;
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
            let conn = rusqlite::Connection::open(dir.path().join("c.db")).unwrap();
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
        let cache = LinkCache::new(
            crate::db::open_store(dir.path().join("c.db").to_str().unwrap()).unwrap(),
        );
        cache.put("twitter:1", &cached_photo()).await;
        cache.put("pixiv:2", &cached_photo()).await;
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
