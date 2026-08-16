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
static DB: LazyLock<Arc<db::DbPool>> = LazyLock::new(|| {
    let path = db_path();
    db::open_store(&path.to_string_lossy()).expect("failed to open database")
});

/// DB file location: `$DATA_DIR/task_queue.db` (default `data`, relative to
/// the working directory — keeps the docker-compose `./data` mount and local
/// runs unchanged). The directory is created if missing: SQLite does not
/// create parent dirs, so the old hardcoded `data/task_queue.db` failed with
/// a confusing error when started from a directory without `data/`, and a
/// CWD-relative path is a footgun for systemd / cron deployments — `DATA_DIR`
/// lets them pin the state anywhere.
fn db_path() -> std::path::PathBuf {
    let dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let dir_path = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir_path).expect("failed to create data directory");
    dir_path.join("task_queue.db")
}

pub static CHAT_STORE: LazyLock<ChatStore> = LazyLock::new(|| ChatStore::new(Arc::clone(&DB)));
pub static TASK_QUEUE: LazyLock<PersistentTaskQueue> =
    LazyLock::new(|| PersistentTaskQueue::new(Arc::clone(&DB)));
pub static LINK_CACHE: LazyLock<LinkCache> = LazyLock::new(|| LinkCache::new(Arc::clone(&DB)));
pub static CONFIG: LazyLock<Config> = LazyLock::new(Config::load);
