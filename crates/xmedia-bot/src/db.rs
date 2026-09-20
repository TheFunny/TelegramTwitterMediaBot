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

/// Opens the shared DB file, runs the merged schema for all three tables and
/// returns a pool for it. One call per process in production (the stores
/// share the returned pool); tests call it per tempdir.
pub fn open_store(path: &str) -> rusqlite::Result<Arc<DbPool>> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(rusqlite_error)?;
    }
    let conn = open_db(path)?;
    schema_init(&conn)?;
    migrate(&conn)?;
    Ok(Arc::new(DbPool::new(path)))
}

/// Schema migrations, applied in order and tracked by `PRAGMA user_version`
/// (the index in this array + 1 is the version a statement brings the
/// database to). Append only — never edit or reorder an entry, or databases
/// already past it would skip or repeat work.
const MIGRATIONS: &[&str] = &[
    // 1: lease fencing. A worker's write-backs (`delete`/`reschedule`/the
    // lease heartbeat) are guarded by the token it was leased with, so a
    // lease that expired and was re-leased by another worker can no longer be
    // written by its former holder — which used to duplicate a send or drop
    // the new holder's retry state, silently.
    "ALTER TABLE tasks ADD COLUMN lease_token TEXT",
];

/// Brings an existing database up to [`MIGRATIONS`]. Idempotent: a database
/// already at the latest version does no work.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (index, statement) in MIGRATIONS.iter().enumerate() {
        let target = index as i64 + 1;
        if version >= target {
            continue;
        }
        conn.execute_batch(statement)?;
        // `PRAGMA` does not take bind parameters; the value is our own index.
        conn.execute_batch(&format!("PRAGMA user_version = {target}"))?;
    }
    Ok(())
}

fn rusqlite_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// Creates the `tasks`, `chat_state` and `link_cache` tables (idempotent).
/// The three stores used to own their own schema; keeping it in one place
/// means one initialization for the whole database file.
///
/// This is the **baseline** schema (version 0): a fresh database is created
/// exactly like this, and anything that must *change* an existing one is
/// appended to [`MIGRATIONS`] instead of being edited in here — otherwise a
/// database created before the change would never gain the new column and a
/// freshly created one would try to apply the migration a second time.
pub fn schema_init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, payload TEXT NOT NULL, \
         run_after REAL NOT NULL, attempts INTEGER NOT NULL, status TEXT NOT NULL, \
         locked_until REAL NOT NULL, created_at REAL NOT NULL); \
         CREATE INDEX IF NOT EXISTS idx_tasks_pending ON tasks(status, run_after); \
         CREATE TABLE IF NOT EXISTS chat_state (chat_id TEXT PRIMARY KEY, payload TEXT NOT NULL); \
         CREATE TABLE IF NOT EXISTS link_cache (url TEXT PRIMARY KEY, payload TEXT NOT NULL, \
         created_at REAL NOT NULL);",
    )
}

/// Unix timestamp in fractional seconds. Shared by the queue, chat store and
/// link cache (previously four private copies).
pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Unix timestamp in whole seconds. Same clock as [`now_f64`], for fields
/// that store integer seconds (chat-state expiry, edit prompts).
pub fn unix_now() -> i64 {
    now_f64() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// The schema as it shipped *before* the first migration: what an existing
    /// deployment has on disk when it starts on the new binary. Written out
    /// literally rather than derived from `schema_init`, so an edit to the
    /// baseline shows up here instead of being followed silently.
    const V0_SCHEMA: &str = "CREATE TABLE tasks (id TEXT PRIMARY KEY, payload TEXT NOT NULL, \
         run_after REAL NOT NULL, attempts INTEGER NOT NULL, status TEXT NOT NULL, \
         locked_until REAL NOT NULL, created_at REAL NOT NULL); \
         CREATE INDEX idx_tasks_pending ON tasks(status, run_after); \
         CREATE TABLE chat_state (chat_id TEXT PRIMARY KEY, payload TEXT NOT NULL); \
         CREATE TABLE link_cache (url TEXT PRIMARY KEY, payload TEXT NOT NULL, \
         created_at REAL NOT NULL);";

    /// The migrations that have already shipped, verbatim. Appending is the only
    /// allowed change: editing one that a database has already applied leaves
    /// deployments on different schemas with nothing to notice it — the version
    /// counter says "done" and skips the new text.
    const SHIPPED_MIGRATIONS: &[&str] = &["ALTER TABLE tasks ADD COLUMN lease_token TEXT"];

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let mut names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        names.sort();
        names
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn a_pre_migration_database_upgrades_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V0_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES ('task_old', '{\"chat_id\":1}', 0, 0, 'pending', 0, 0)",
                [],
            )
            .unwrap();
            assert_eq!(user_version(&conn), 0, "the fixture starts un-migrated");
            assert!(
                !columns(&conn, "tasks").contains(&"lease_token".to_string()),
                "the fixture is the pre-migration shape"
            );
        }

        let pool = open_store(path.to_str().unwrap()).unwrap();
        pool.with_conn(|conn| {
            assert_eq!(user_version(conn), MIGRATIONS.len() as i64);
            let mut expected = vec![
                "id",
                "payload",
                "run_after",
                "attempts",
                "status",
                "locked_until",
                "created_at",
                "lease_token",
            ];
            expected.sort();
            assert_eq!(
                columns(conn, "tasks"),
                expected,
                "an upgrade must add the migration's column and nothing else"
            );
            let payload: String = conn
                .query_row(
                    "SELECT payload FROM tasks WHERE id = 'task_old'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(payload, "{\"chat_id\":1}", "rows survive the upgrade");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn shipped_migrations_are_frozen() {
        assert!(
            MIGRATIONS.len() >= SHIPPED_MIGRATIONS.len(),
            "migrations were removed or reordered, not appended"
        );
        for (index, (shipped, current)) in SHIPPED_MIGRATIONS.iter().zip(MIGRATIONS).enumerate() {
            assert_eq!(
                shipped,
                current,
                "migration {} already shipped: append a new one instead of editing it",
                index + 1
            );
        }
    }

    #[tokio::test]
    async fn a_fresh_database_lands_at_the_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.db");
        let pool = open_store(path.to_str().unwrap()).unwrap();
        // Every migration is applied on creation, so a deployment that only ever
        // saw fresh databases is on the same schema as an upgraded one.
        pool.with_conn(|conn| {
            assert_eq!(user_version(conn), MIGRATIONS.len() as i64);
            Ok(())
        })
        .await
        .unwrap();
        // Opening the same file again is a no-op (the version gate skips it).
        open_store(path.to_str().unwrap()).unwrap();
    }
}
