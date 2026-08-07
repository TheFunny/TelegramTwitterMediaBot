//! Generic persistent task queue backed by SQLite (table `tasks`).
//!
//! Concepts kept from the Python `utils/task_queue.py` (untrusted, redesigned):
//! the table schema, the lease/lock/recovery model, and the retry→dead-letter
//! flow. The Python dict-mutation hack (attempts inside the payload) is
//! replaced by dedicated columns.

use parking_lot::Mutex;
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const MAX_RETRIES: u32 = 2;
pub const LOCK_TTL_SECONDS: f64 = 120.0;

/// Number of concurrent worker loops. Tasks are independent (retries and
/// forward resumes); leases serialize row claims via SQLite transactions, so
/// extra workers drain backlogs faster. Each worker can be mid-send to
/// Telegram at the same time as handler tasks, so keep this modest.
const QUEUE_WORKERS: usize = 4;

/// What a handler returns instead of throwing. The payload it carries is the
/// (possibly updated) task state to persist for the next attempt.
pub enum QueueError {
    /// Reschedule with the given delay; after `MAX_RETRIES` attempts the task
    /// is dead-lettered instead.
    Retryable { delay_seconds: f64, payload: Value },
    /// Give up now.
    Permanent { message: String, payload: Value },
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Handler = dyn Fn(Value) -> BoxFuture<'static, Result<(), QueueError>> + Send + Sync;
type DeadLetter = dyn Fn(Value, String) -> BoxFuture<'static, ()> + Send + Sync;

pub struct PersistentTaskQueue {
    db_path: String,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Vec<JoinHandle<()>>>,
    counter: AtomicU64,
}

struct LeasedRow {
    id: String,
    payload: String,
    attempts: i32,
}

/// Owned worker state so the spawned loop does not borrow the queue handle.
struct QueueWorker {
    db_path: String,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
    handler: Arc<Handler>,
    dead_letter: Arc<DeadLetter>,
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, payload TEXT NOT NULL, \
         run_after REAL NOT NULL, attempts INTEGER NOT NULL, status TEXT NOT NULL, \
         locked_until REAL NOT NULL, created_at REAL NOT NULL);",
    )
}

impl PersistentTaskQueue {
    pub fn new(db_path: &str) -> Self {
        // Ensure the parent dir and table exist even if only the queue (not
        // ChatStore) is used — a fresh container without a mounted data dir
        // must still be able to open the DB.
        if let Some(parent) = std::path::Path::new(db_path).parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            log::error!("failed to create queue dir: {e}");
        }
        if let Ok(conn) = Connection::open(db_path)
            && let Err(e) = ensure_schema(&conn)
        {
            log::error!("failed to initialize queue schema: {e}");
        }
        Self {
            db_path: db_path.to_string(),
            notify: Arc::new(Notify::new()),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(Vec::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Starts the worker loops. Also recovers rows left `in_progress` by a
    /// previous process (lease expired).
    pub async fn start<H, F, D, G>(&self, handler: H, dead_letter: D)
    where
        H: Fn(Value) -> F + Send + Sync + 'static,
        F: Future<Output = Result<(), QueueError>> + Send + 'static,
        D: Fn(Value, String) -> G + Send + Sync + 'static,
        G: Future<Output = ()> + Send + 'static,
    {
        let handler: Arc<Handler> = Arc::new(move |payload| Box::pin(handler(payload)));
        let dead_letter: Arc<DeadLetter> =
            Arc::new(move |payload, message| Box::pin(dead_letter(payload, message)));
        self.recover_stale().await;
        let mut handles = Vec::with_capacity(QUEUE_WORKERS);
        for _ in 0..QUEUE_WORKERS {
            let worker = QueueWorker {
                db_path: self.db_path.clone(),
                notify: Arc::clone(&self.notify),
                stop: Arc::clone(&self.stop),
                handler: Arc::clone(&handler),
                dead_letter: Arc::clone(&dead_letter),
            };
            handles.push(tokio::spawn(worker.run_loop()));
        }
        *self.worker.lock() = handles;
    }

    pub async fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
        let handles = std::mem::take(&mut *self.worker.lock());
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Persists a task. `run_after` is an absolute unix timestamp (seconds).
    /// Notifies the worker only after the insert has committed, so the worker
    /// never wakes to an invisible row.
    pub async fn enqueue(&self, payload: Value, run_after: f64) -> rusqlite::Result<()> {
        let id = format!(
            "task_{}_{}",
            (now_f64() * 1000.0) as u64,
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let payload = payload.to_string();
        log::info!("enqueued {id} (run_after {run_after:.1})");
        crate::db::with_conn(&self.db_path, move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES (?1, ?2, ?3, 0, 'pending', 0, ?4)",
                params![id, payload, run_after, now_f64()],
            )?;
            Ok(())
        })
        .await?;
        // Wake every sleeping worker: with several workers the one that finds
        // nothing due must not starve the newly inserted row.
        self.notify.notify_waiters();
        Ok(())
    }

    async fn recover_stale(&self) {
        let result = crate::db::with_conn(&self.db_path, move |conn| {
            conn.execute(
                "UPDATE tasks SET status='pending', locked_until=0 WHERE status='in_progress' AND locked_until < ?1",
                params![now_f64()],
            )?;
            Ok(())
        })
        .await;
        if let Err(e) = result {
            log::error!("queue recovery failed: {e}");
        }
    }
}

impl QueueWorker {
    async fn run_loop(self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.lease_next().await {
                Some(row) => self.process(row).await,
                None => {
                    let wait_until = self.earliest_run_after().await;
                    let notified = self.notify.notified();
                    tokio::pin!(notified);
                    match wait_until {
                        Some(until) => {
                            let delay = (until - now_f64()).max(0.0);
                            tokio::select! {
                                _ = &mut notified => {}
                                _ = tokio::time::sleep(Duration::from_secs_f64(delay)) => {}
                            }
                        }
                        None => {
                            notified.await;
                        }
                    }
                }
            }
        }
    }

    /// Leases the oldest due row (sets it `in_progress` with a lock TTL).
    async fn lease_next(&self) -> Option<LeasedRow> {
        let result = crate::db::with_conn(&self.db_path, |conn| {
            // BEGIN IMMEDIATE: with several workers, a deferred transaction
            // that read before another worker's lease commit would fail with
            // SQLITE_BUSY_SNAPSHOT. Taking the write lock up front serializes
            // leases and re-reads the freshest committed state.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = now_f64();
            let row = tx.query_row(
                "SELECT id, payload, attempts FROM tasks WHERE status='pending' AND run_after <= ?1 AND locked_until <= ?1 \
                 ORDER BY run_after LIMIT 1",
                params![now],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i32>(2)?,
                    ))
                },
            );
            let (id, payload, attempts) = match row {
                Ok(row) => row,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    tx.commit()?;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            tx.execute(
                "UPDATE tasks SET status='in_progress', locked_until=?1 WHERE id=?2",
                params![now + LOCK_TTL_SECONDS, id],
            )?;
            tx.commit()?;
            Ok(Some(LeasedRow {
                id,
                payload,
                attempts,
            }))
        })
        .await;
        match result {
            Ok(row) => row,
            Err(e) => {
                log::error!("queue lease failed: {e}");
                None
            }
        }
    }

    async fn earliest_run_after(&self) -> Option<f64> {
        let result = crate::db::with_conn(&self.db_path, |conn| {
            let mut stmt =
                conn.prepare("SELECT MIN(run_after) FROM tasks WHERE status='pending'")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, Option<f64>>(0)?),
                None => Ok(None),
            }
        })
        .await;
        match result {
            Ok(v) => v,
            Err(e) => {
                log::error!("queue timing query failed: {e}");
                None
            }
        }
    }

    async fn process(&self, row: LeasedRow) {
        let payload: Value = match serde_json::from_str(&row.payload) {
            Ok(value) => value,
            Err(e) => {
                log::error!("queue: unparseable payload for {}: {e}", row.id);
                self.delete_row(&row.id).await;
                (self.dead_letter)(Value::Null, format!("invalid stored payload: {e}")).await;
                return;
            }
        };
        log::info!("processing {} (attempt {})", row.id, row.attempts + 1);
        match (self.handler)(payload).await {
            Ok(()) => {
                log::info!("task {} completed", row.id);
                self.delete_row(&row.id).await;
            }
            Err(QueueError::Retryable {
                delay_seconds,
                payload,
            }) => {
                if row.attempts as u32 >= MAX_RETRIES {
                    let message = format!("task failed after {MAX_RETRIES} retries");
                    log::error!("dead-lettering {}: {message}", row.id);
                    self.delete_row(&row.id).await;
                    (self.dead_letter)(payload, message).await;
                } else {
                    log::info!(
                        "task {} rescheduled in {delay_seconds:.1}s (attempt {})",
                        row.id,
                        row.attempts + 1
                    );
                    self.reschedule(&row.id, payload, delay_seconds, row.attempts + 1)
                        .await;
                }
            }
            Err(QueueError::Permanent { message, payload }) => {
                log::error!("dead-lettering {}: {message}", row.id);
                self.delete_row(&row.id).await;
                (self.dead_letter)(payload, message).await;
            }
        }
    }

    async fn delete_row(&self, id: &str) {
        let id = id.to_string();
        let result = crate::db::with_conn(&self.db_path, move |conn| {
            conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await;
        if let Err(e) = result {
            log::error!("queue delete failed: {e}");
        }
    }

    async fn reschedule(&self, id: &str, payload: Value, delay_seconds: f64, attempts: i32) {
        let id = id.to_string();
        let payload = payload.to_string();
        let result = crate::db::with_conn(&self.db_path, move |conn| {
            conn.execute(
                "UPDATE tasks SET payload=?1, run_after=?2, attempts=?3, status='pending', locked_until=0 WHERE id=?4",
                params![payload, now_f64() + delay_seconds, attempts, id],
            )?;
            Ok(())
        })
        .await;
        if let Err(e) = result {
            log::error!("queue reschedule failed: {e}");
        }
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    async fn new_queue() -> (PersistentTaskQueue, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let queue = PersistentTaskQueue::new(path.to_str().unwrap());
        (queue, dir)
    }

    #[tokio::test]
    async fn enqueue_runs_handler_once() {
        let (queue, _dir) = new_queue().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_worker = calls.clone();
        queue
            .start(
                move |payload| {
                    assert_eq!(payload["n"], 42);
                    calls_worker.fetch_add(1, AtomicOrdering::SeqCst);
                    async { Ok(()) }
                },
                |_payload, _message| async {},
            )
            .await;
        queue
            .enqueue(serde_json::json!({"n": 42}), now_f64())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        queue.stop().await;
    }

    #[tokio::test]
    async fn retryable_reschedules_then_dead_letters() {
        let (queue, _dir) = new_queue().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let dead_calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let d = dead_calls.clone();
        queue
            .start(
                move |payload| {
                    c.fetch_add(1, AtomicOrdering::SeqCst);
                    let payload = payload.clone();
                    async move {
                        Err(QueueError::Retryable {
                            delay_seconds: 0.001,
                            payload,
                        })
                    }
                },
                move |_payload, _message| {
                    d.fetch_add(1, AtomicOrdering::SeqCst);
                    async {}
                },
            )
            .await;
        queue
            .enqueue(serde_json::json!({"a": 1}), now_f64())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            MAX_RETRIES as usize + 1,
            "handler should run once per attempt"
        );
        assert_eq!(dead_calls.load(AtomicOrdering::SeqCst), 1);
        queue.stop().await;
    }

    #[tokio::test]
    async fn permanent_error_dead_letters_immediately() {
        let (queue, _dir) = new_queue().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let dead_calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let d = dead_calls.clone();
        queue
            .start(
                move |payload| {
                    c.fetch_add(1, AtomicOrdering::SeqCst);
                    let payload = payload.clone();
                    async move {
                        Err(QueueError::Permanent {
                            message: "nope".into(),
                            payload,
                        })
                    }
                },
                move |_payload, message| {
                    assert_eq!(message, "nope");
                    d.fetch_add(1, AtomicOrdering::SeqCst);
                    async {}
                },
            )
            .await;
        queue
            .enqueue(serde_json::json!({"a": 1}), now_f64())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(dead_calls.load(AtomicOrdering::SeqCst), 1);
        queue.stop().await;
    }

    #[tokio::test]
    async fn stale_in_progress_row_is_recovered_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        // Insert a stale leased row directly (lease expired).
        {
            let conn = Connection::open(&path).unwrap();
            ensure_schema(&conn).unwrap();
            conn.execute(
                "INSERT INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES ('task_stale', '{\"s\":1}', 0, 0, 'in_progress', ?1, 0)",
                params![now_f64() - 10.0],
            )
            .unwrap();
        }
        let queue = PersistentTaskQueue::new(path.to_str().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        queue
            .start(
                move |payload| {
                    assert_eq!(payload["s"], 1);
                    c.fetch_add(1, AtomicOrdering::SeqCst);
                    async { Ok(()) }
                },
                |_payload, _message| async {},
            )
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        queue.stop().await;
    }
}
