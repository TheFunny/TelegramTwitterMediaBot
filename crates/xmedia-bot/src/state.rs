//! Per-chat state with SQLite persistence (table `chat_state` in
//! `data/task_queue.db`, shared with the task queue).

use parking_lot::Mutex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ChatData {
    pub forward_channel_id: Option<i64>,
    pub edit_before_forward: bool,
    /// Key: prompt message id.
    pub edit_message: HashMap<i64, EditMessage>,
    /// name -> HTML template containing "[]"
    pub template: HashMap<String, String>,
    /// site name (twitter/bsky/pixiv) -> user-supplied caption format with
    /// {url} {author} {author_url} {title} {tags} placeholders.
    pub message_format: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct EditMessage {
    pub url: String,
    pub chat_id: i64,
    pub forward_message_ids: Vec<i64>,
    pub template: String,
    /// Unix seconds at registration; expiry = created_at + ttl.
    pub created_at: i64,
}

pub struct ChatStore {
    /// In-memory cache; the DB is the source of truth on first access.
    cache: Mutex<HashMap<i64, ChatData>>,
    /// Per-chat async locks serializing get→mutate→set so concurrent handler
    /// tasks (batch-forwards, callbacks) cannot clobber each other's writes.
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    db_path: String,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ChatStore {
    /// Creates the parent directory and the `chat_state` table (idempotent).
    /// The shared `tasks` / `link_cache` tables are owned by `queue.rs` and
    /// `link_cache.rs` respectively.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = crate::db::open_db(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chat_state (chat_id TEXT PRIMARY KEY, payload TEXT NOT NULL);",
        )?;
        drop(conn);
        Ok(ChatStore {
            cache: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            db_path: path.to_string(),
        })
    }

    pub async fn get(&self, chat_id: i64) -> ChatData {
        if let Some(data) = self.cache.lock().get(&chat_id) {
            return data.clone();
        }
        let chat_key = chat_id.to_string();
        let payload = crate::db::with_conn(&self.db_path, move |conn| {
            // Concurrent handler tasks (batch-forwards) may write chat_state
            // while this read runs; the shared busy timeout handles the
            // write-lock collision instead of failing the query.
            let mut stmt = conn.prepare("SELECT payload FROM chat_state WHERE chat_id = ?1")?;
            let mut rows = stmt.query(params![chat_key])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get::<_, String>(0)?)),
                None => Ok(None),
            }
        })
        .await
        .unwrap_or_else(|e| {
            log::error!("chat_state read failed: {e}");
            None
        })
        .unwrap_or_default();
        let data: ChatData = serde_json::from_str(&payload).unwrap_or_default();
        self.cache.lock().insert(chat_id, data.clone());
        data
    }

    /// Write-through: update the cache and the DB.
    pub async fn set(&self, chat_id: i64, data: &ChatData) {
        self.cache.lock().insert(chat_id, data.clone());
        let payload = serde_json::to_string(data).expect("chat state serializes");
        let chat_id = chat_id.to_string();
        let result = crate::db::with_conn(&self.db_path, move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO chat_state (chat_id, payload) VALUES (?1, ?2)",
                params![chat_id, payload],
            )?;
            Ok(())
        })
        .await;
        if let Err(e) = result {
            log::error!("chat_state write failed: {e}");
        }
    }

    /// Serializes a get→mutate→set cycle per chat: concurrent handler tasks
    /// (the batch-forward design spawns several per chat) each snapshot the
    /// same `ChatData` and last-writer-wins would silently drop mutations,
    /// e.g. a second `edit_message` record. The per-chat lock makes the
    /// cycle atomic. Returns the closure's result.
    pub async fn update<R>(&self, chat_id: i64, f: impl FnOnce(&mut ChatData) -> R) -> R {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(chat_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let mut data = self.get(chat_id).await;
        let r = f(&mut data);
        self.set(chat_id, &data).await;
        r
    }

    /// Removes edit-before-forward records whose `created_at + ttl` is in the
    /// past. Returns the removed `(chat_id, prompt_message_id)` pairs so the
    /// caller can clear the prompt's buttons.
    pub async fn prune_expired(&self, ttl: Duration) -> Vec<(i64, i64)> {
        let now = unix_now();
        let ttl_secs = ttl.as_secs() as i64;
        let mut removed = Vec::new();
        let changed: Vec<(i64, ChatData)> = {
            let mut cache = self.cache.lock();
            let mut out = Vec::new();
            for (chat_id, data) in cache.iter_mut() {
                let keys: Vec<i64> = data.edit_message.keys().copied().collect();
                let mut kept = HashMap::new();
                for key in keys {
                    if let Some(entry) = data.edit_message.get(&key) {
                        if entry.created_at + ttl_secs > now {
                            kept.insert(key, entry.clone());
                        } else {
                            removed.push((*chat_id, key));
                        }
                    }
                }
                if kept.len() != data.edit_message.len() {
                    data.edit_message = kept;
                    out.push((*chat_id, data.clone()));
                }
            }
            out
        };
        for (chat_id, data) in changed {
            self.set(chat_id, &data).await;
        }
        if !removed.is_empty() {
            log::info!(
                "pruned {} expired edit-before-forward record(s)",
                removed.len()
            );
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_updates_do_not_lose_edit_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            ChatStore::open(dir.path().join("s.db").to_str().unwrap()).unwrap(),
        );
        let mut handles = Vec::new();
        for i in 0..4 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                store
                    .update(1001, |data| {
                        data.edit_message.insert(
                            i,
                            EditMessage {
                                url: format!("https://x.com/u/status/{i}"),
                                chat_id: 1001,
                                forward_message_ids: vec![i],
                                template: String::new(),
                                created_at: 0,
                            },
                        );
                    })
                    .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let data = store.get(1001).await;
        assert_eq!(
            data.edit_message.len(),
            4,
            "concurrent get→mutate→set must not drop records"
        );
    }
}
