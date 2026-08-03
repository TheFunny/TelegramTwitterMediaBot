//! Per-chat state with SQLite persistence (table `chat_state` in
//! `data/task_queue.db`, shared with the task queue).

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
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
    db_path: String,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ChatStore {
    /// Creates the parent directory and both tables (idempotent).
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, payload TEXT NOT NULL, \
             run_after REAL NOT NULL, attempts INTEGER NOT NULL, status TEXT NOT NULL, \
             locked_until REAL NOT NULL, created_at REAL NOT NULL); \
             CREATE TABLE IF NOT EXISTS chat_state (chat_id TEXT PRIMARY KEY, payload TEXT NOT NULL);",
        )?;
        drop(conn);
        Ok(ChatStore {
            cache: Mutex::new(HashMap::new()),
            db_path: path.to_string(),
        })
    }

    pub async fn get(&self, chat_id: i64) -> ChatData {
        if let Some(data) = self.cache.lock().get(&chat_id) {
            return data.clone();
        }
        let db_path = self.db_path.clone();
        let payload = tokio::task::spawn_blocking(move || -> rusqlite::Result<Option<String>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare("SELECT payload FROM chat_state WHERE chat_id = ?1")?;
            let mut rows = stmt.query(params![chat_id.to_string()])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
        .await
        .expect("chat_state worker panicked")
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
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = Connection::open(&db_path)?;
            conn.execute(
                "INSERT OR REPLACE INTO chat_state (chat_id, payload) VALUES (?1, ?2)",
                params![chat_id.to_string(), payload],
            )?;
            Ok(())
        })
        .await
        .expect("chat_state worker panicked")
        .unwrap_or_else(|e| log::error!("chat_state write failed: {e}"));
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
            log::info!("pruned {} expired edit-before-forward record(s)", removed.len());
        }
        removed
    }
}
