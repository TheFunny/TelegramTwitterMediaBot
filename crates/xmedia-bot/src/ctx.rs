//! Runtime context: the collaborators a handler needs, injected as one struct
//! so tests can substitute a scripted sender and tempdir-backed stores.
//!
//! The production context is assembled from the process-wide statics
//! ([`AppContext::from_statics`]); the spawned worker closures hold
//! [`CONTEXT`], which is `'static` for that reason.

use crate::config::Config;
use crate::handlers::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE};
use crate::link_cache::LinkCache;
use crate::media_sender::MediaSender;
use crate::queue::PersistentTaskQueue;
use crate::send::BOT;
use crate::state::ChatStore;
use std::sync::LazyLock;

pub struct AppContext<'a> {
    pub sender: &'a dyn MediaSender,
    pub chat_store: &'a ChatStore,
    pub task_queue: &'a PersistentTaskQueue,
    pub link_cache: &'a LinkCache,
    pub config: &'a Config,
}

impl<'a> AppContext<'a> {
    /// The stores are the process-wide statics; `sender` is whatever the caller
    /// was handed (the dispatcher's `Bot` clone for update handlers, the shared
    /// queue `Bot` for the worker loops). Update handlers build their own
    /// context from the `Bot` they received so the same code path works with an
    /// injected mock in tests.
    pub fn from_statics(sender: &'a dyn MediaSender) -> AppContext<'a> {
        AppContext {
            sender,
            chat_store: &CHAT_STORE,
            task_queue: &TASK_QUEUE,
            link_cache: &LINK_CACHE,
            config: &CONFIG,
        }
    }
}

/// The URL/queue workers' context: `'static` because `tokio::spawn`ed closures
/// and the queue's handler type require it.
pub static CONTEXT: LazyLock<AppContext<'static>> =
    LazyLock::new(|| AppContext::from_statics(&*BOT));

/// Test support: a tempdir-backed set of stores plus the context borrowing
/// them, so a handler test needs one line of setup.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Arc;

    pub(crate) struct TestStores {
        _dir: tempfile::TempDir,
        pool: Arc<crate::db::DbPool>,
        chat_store: ChatStore,
        task_queue: PersistentTaskQueue,
        link_cache: LinkCache,
        config: Config,
    }

    impl TestStores {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let pool = crate::db::open_store(dir.path().join("ctx.db").to_str().unwrap()).unwrap();
            TestStores {
                _dir: dir,
                chat_store: ChatStore::new(Arc::clone(&pool)),
                task_queue: PersistentTaskQueue::new(Arc::clone(&pool)),
                link_cache: LinkCache::new(Arc::clone(&pool)),
                config: Config::load(),
                pool,
            }
        }

        pub(crate) fn ctx<'a>(&'a self, sender: &'a dyn MediaSender) -> AppContext<'a> {
            AppContext {
                sender,
                chat_store: &self.chat_store,
                task_queue: &self.task_queue,
                link_cache: &self.link_cache,
                config: &self.config,
            }
        }

        pub(crate) fn chat_store(&self) -> &ChatStore {
            &self.chat_store
        }

        /// The parsed config, mutable so a test can pin a knob (e.g. the
        /// caption-quote threshold) instead of depending on the environment.
        pub(crate) fn config_mut(&mut self) -> &mut Config {
            &mut self.config
        }

        pub(crate) fn link_cache(&self) -> &LinkCache {
            &self.link_cache
        }

        /// Rows persisted in the task queue: what "queued for retry" looks like
        /// from the outside.
        pub(crate) async fn queued_tasks(&self) -> i64 {
            let pool = Arc::clone(&self.pool);
            pool.with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            })
            .await
            .unwrap()
        }

        /// The single queued task payload, for asserting what was rescheduled.
        pub(crate) async fn queued_payload(&self) -> serde_json::Value {
            let pool = Arc::clone(&self.pool);
            let payload: String = pool
                .with_conn(|conn| {
                    conn.query_row("SELECT payload FROM tasks LIMIT 1", [], |row| row.get(0))
                })
                .await
                .unwrap();
            serde_json::from_str(&payload).unwrap()
        }
    }
}
