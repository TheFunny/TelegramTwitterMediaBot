//! Per-chat state with SQLite persistence (table `chat_state` in
//! `data/task_queue.db`, shared with the task queue).

use crate::db::unix_now;
use parking_lot::Mutex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ChatData {
    pub forward_channel_id: Option<i64>,
    pub edit_before_forward: bool,
    /// Key: prompt message id.
    pub edit_message: HashMap<i64, EditMessage>,
    /// name -> HTML template containing "[]"
    pub template: HashMap<String, String>,
    /// site name (twitter/bsky/misskey/pixiv/bilibili) -> user-supplied caption format
    /// with {url} {author} {author_url} {title} {content} {tags} placeholders.
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
    pool: Arc<crate::db::DbPool>,
}

impl ChatStore {
    /// Wraps the shared DB pool (schema initialized once by
    /// [`crate::db::open_store`]; the `chat_state` table lives in the merged
    /// schema alongside `tasks` and `link_cache`).
    pub fn new(pool: Arc<crate::db::DbPool>) -> Self {
        ChatStore {
            cache: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            pool,
        }
    }

    pub async fn get(&self, chat_id: i64) -> ChatData {
        if let Some(data) = self.cache.lock().get(&chat_id) {
            return data.clone();
        }
        let chat_key = chat_id.to_string();
        let payload = self
            .pool
            .with_conn(move |conn| {
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
        let result = self
            .pool
            .with_conn(move |conn| {
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

    /// The per-chat async lock serializing get→mutate→set cycles.
    fn lock_for(&self, chat_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .entry(chat_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Serializes a get→mutate→set cycle per chat: concurrent handler tasks
    /// (the batch-forward design spawns several per chat) each snapshot the
    /// same `ChatData` and last-writer-wins would silently drop mutations,
    /// e.g. a second `edit_message` record. The per-chat lock makes the
    /// cycle atomic. Returns the closure's result.
    pub async fn update<R>(&self, chat_id: i64, f: impl FnOnce(&mut ChatData) -> R) -> R {
        let lock = self.lock_for(chat_id);
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
        // Chats that may have an expired record, from a cache snapshot; the
        // pruning itself re-reads and writes under the per-chat lock below
        // (see the eviction note). Takes no lock of its own, so a chat
        // appearing later is simply picked up by the next sweep.
        let candidates: Vec<i64> = {
            let cache = self.cache.lock();
            cache
                .iter()
                .filter(|(_, data)| {
                    data.edit_message
                        .values()
                        .any(|entry| entry.created_at + ttl_secs <= now)
                })
                .map(|(chat_id, _)| *chat_id)
                .collect()
        };
        let mut removed = Vec::new();
        let mut evicted_chats = Vec::new();
        for chat_id in candidates {
            let lock = self.lock_for(chat_id);
            let _guard = lock.lock().await;
            let mut data = self.get(chat_id).await;
            let before = data.edit_message.len();
            data.edit_message.retain(|key, entry| {
                if entry.created_at + ttl_secs > now {
                    return true;
                }
                removed.push((chat_id, *key));
                false
            });
            if data.edit_message.len() != before {
                self.set(chat_id, &data).await;
            }
            // Chats with no live edit records: evicted from the cache (and
            // their per-chat lock) so the cache stays bounded to active
            // prompts. The DB keeps the row; the next get() reloads it.
            if data.edit_message.is_empty() {
                evicted_chats.push(chat_id);
            }
        }
        if !evicted_chats.is_empty() {
            let mut cache = self.cache.lock();
            let mut locks = self.locks.lock();
            for chat_id in &evicted_chats {
                cache.remove(chat_id);
                locks.remove(chat_id);
            }
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
        let pool = crate::db::open_store(dir.path().join("s.db").to_str().unwrap()).unwrap();
        let store = std::sync::Arc::new(ChatStore::new(pool));
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

    fn edit_entry(chat_id: i64, created_at: i64) -> EditMessage {
        EditMessage {
            url: "https://x.com/u/status/1".into(),
            chat_id,
            forward_message_ids: vec![9],
            template: String::new(),
            created_at,
        }
    }

    #[tokio::test]
    async fn prune_removes_only_expired_records() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_store(dir.path().join("p.db").to_str().unwrap()).unwrap();
        let store = ChatStore::new(pool);
        let now = unix_now();
        store
            .update(7, |data| {
                data.template.insert("t".into(), "[]".into());
                data.edit_message.insert(1, edit_entry(7, now - 3600));
                data.edit_message.insert(2, edit_entry(7, now));
            })
            .await;

        let removed = store.prune_expired(Duration::from_secs(60)).await;

        assert_eq!(removed, vec![(7, 1)]);
        let data = store.get(7).await;
        assert!(data.edit_message.contains_key(&2), "live record pruned");
        assert_eq!(
            data.template.get("t").map(String::as_str),
            Some("[]"),
            "unrelated state lost by the prune"
        );
    }

    #[tokio::test]
    async fn prune_eviction_keeps_the_persisted_state() {
        // Every record expires → the chat is evicted from the cache; the
        // pruned state must already be in the DB when that happens.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_store(dir.path().join("p.db").to_str().unwrap()).unwrap();
        let store = ChatStore::new(pool);
        store
            .update(8, |data| {
                data.template.insert("keep".into(), "[]".into());
                data.edit_message.insert(1, edit_entry(8, 0));
            })
            .await;

        let removed = store.prune_expired(Duration::from_secs(60)).await;

        assert_eq!(removed, vec![(8, 1)]);
        let data = store.get(8).await;
        assert!(data.edit_message.is_empty());
        assert_eq!(
            data.template.get("keep").map(String::as_str),
            Some("[]"),
            "eviction dropped state the DB never received"
        );
    }
}
