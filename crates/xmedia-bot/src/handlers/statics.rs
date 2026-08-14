//! Process-wide singletons shared by the handler modules: the one SQLite
//! pool (and the three stores built on it) plus the configuration.

use crate::config::Config;
use crate::db::{self};
use crate::link_cache::LinkCache;
use crate::queue::PersistentTaskQueue;
use crate::state::ChatStore;
use std::sync::{Arc, LazyLock};

/// One shared SQLite pool for the three stores (chat state, task queue, link
/// cache): a single pool bounds concurrent DB work on `data/task_queue.db`
/// instead of three independent pools competing for the same file. The schema
/// for all three tables is initialized once, here.
static DB: LazyLock<Arc<db::DbPool>> =
    LazyLock::new(|| db::open_store("data/task_queue.db").expect("failed to open database"));

pub static CHAT_STORE: LazyLock<ChatStore> = LazyLock::new(|| ChatStore::new(Arc::clone(&DB)));
pub static TASK_QUEUE: LazyLock<PersistentTaskQueue> =
    LazyLock::new(|| PersistentTaskQueue::new(Arc::clone(&DB)));
pub static LINK_CACHE: LazyLock<LinkCache> = LazyLock::new(|| LinkCache::new(Arc::clone(&DB)));
pub static CONFIG: LazyLock<Config> = LazyLock::new(Config::load);
