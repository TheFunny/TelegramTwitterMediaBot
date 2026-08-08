//! Shared SQLite plumbing for the three tables in `data/task_queue.db`
//! (`tasks` in queue.rs, `chat_state` in state.rs, `link_cache` in
//! link_cache.rs).
//!
//! Every operation opens its own short-lived connection with a busy timeout:
//! handler tasks enqueue while workers lease/update rows concurrently, and
//! without the timeout a concurrent write fails immediately with SQLITE_BUSY
//! and the operation is lost. All I/O runs inside `spawn_blocking` via
//! [`with_conn`] — rusqlite connections are not Send-friendly to hold across
//! an await point, and blocking the async executor stalls every handler.

use rusqlite::Connection;
use std::time::Duration;

/// Opens the shared DB with a busy timeout.
pub fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
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

/// Runs `f` against a fresh connection on a blocking thread, returning the
/// closure's result. Owns the `spawn_blocking` + `expect` ceremony shared by
/// every table access; the caller maps errors to its own log line.
pub async fn with_conn<T, F>(path: &str, f: F) -> rusqlite::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = open_db(&path)?;
        f(&mut conn)
    })
    .await
    .expect("db worker panicked")
}
