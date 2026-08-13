//! Shared SQLite plumbing for the three tables in `data/task_queue.db`
//! (`tasks` in queue.rs, `chat_state` in state.rs, `link_cache` in
//! link_cache.rs).
//!
//! All I/O runs inside `spawn_blocking` via [`DbPool::with_conn`] — rusqlite
//! connections are not Send-friendly to hold across an await point, and
//! blocking the async executor stalls every handler. Connections are reused
//! through a small per-store pool instead of opening a fresh connection per
//! operation: WAL lets readers run alongside writer leases, and the pool's
//! semaphore bounds how many DB operations run concurrently, giving natural
//! backpressure on hot paths (every message / URL / callback touches
//! chat_state or the link cache).

use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;

/// Upper bound on pooled (reused) connections and on concurrent DB
/// operations per store. Small on purpose: the queue's `BEGIN IMMEDIATE`
/// leases serialize writes anyway, and WAL readers rarely need more.
const POOL_SIZE: usize = 4;

/// A tiny connection pool for one SQLite file. Connections are checked out
/// on a blocking thread and returned afterwards; `acquire` opens a new
/// connection only when the idle list is empty, so the steady-state cost of
/// an operation is a list pop instead of a fresh open (+ busy timeout + WAL
/// pragma). The semaphore caps the number of concurrent operations, so a
/// burst of handlers queues up instead of opening unbounded connections.
pub struct DbPool {
    // Arc so [`DbPool::with_conn`] can hand an owned handle to
    // `spawn_blocking` without borrowing across the await point.
    inner: Arc<PoolInner>,
}

struct PoolInner {
    path: String,
    permits: tokio::sync::Semaphore,
    idle: Mutex<Vec<Connection>>,
}

impl DbPool {
    pub fn new(path: &str) -> Self {
        DbPool {
            inner: Arc::new(PoolInner {
                path: path.to_string(),
                permits: tokio::sync::Semaphore::new(POOL_SIZE),
                idle: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs `f` against a pooled connection on a blocking thread, returning
    /// the closure's result. Owns the semaphore + `spawn_blocking` +
    /// `expect` ceremony shared by every table access; the caller maps
    /// errors to its own log line.
    pub async fn with_conn<T, F>(&self, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .expect("db pool semaphore closed");
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut conn = inner.acquire()?;
            let result = f(&mut conn);
            inner.release(conn);
            result
        })
        .await
        .expect("db worker panicked")
    }

    /// The database file this pool serves (used by tests that need a raw
    /// connection, e.g. to seed rows directly).
    #[cfg(test)]
    pub fn path(&self) -> &str {
        &self.inner.path
    }
}

impl PoolInner {
    /// Reuses an idle connection or opens a fresh one.
    fn acquire(&self) -> rusqlite::Result<Connection> {
        if let Some(conn) = self.idle.lock().pop() {
            return Ok(conn);
        }
        open_db(&self.path)
    }

    /// Returns a connection to the pool (dropped when the pool is full).
    fn release(&self, conn: Connection) {
        let mut idle = self.idle.lock();
        if idle.len() < POOL_SIZE {
            idle.push(conn);
        }
    }
}

/// Opens the shared DB with a busy timeout.
pub fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    // WAL lets readers run alongside writer leases instead of blocking on
    // the rollback journal; the mode persists in the DB header, so the
    // idempotent pragma here and in ensure_schema only needs to win once.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

/// Unix timestamp in fractional seconds. Shared by the queue, chat store and
/// link cache (previously four private copies).
pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
