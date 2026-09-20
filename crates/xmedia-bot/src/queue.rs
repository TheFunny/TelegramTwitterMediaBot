//! Generic persistent task queue backed by SQLite (table `tasks`).
//!
//! Concepts kept from the Python `utils/task_queue.py` (untrusted, redesigned):
//! the table schema, the lease/lock/recovery model, and the retry→dead-letter
//! flow. The Python dict-mutation hack (attempts inside the payload) is
//! replaced by dedicated columns.

use crate::db::now_f64;
use parking_lot::Mutex;
use rusqlite::{TransactionBehavior, params};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const MAX_RETRIES: u32 = 2;

/// Attempts for a *terminal* row write (delete / reschedule). These are not
/// like a task retry: failing them leaves the row in `in_progress`, where the
/// expiry sweep can re-run a task that already ran, so a contended DB gets a
/// few quick chances before the caller falls back to a terminal state.
const TERMINAL_WRITE_ATTEMPTS: u32 = 3;

/// 100ms, 200ms, … between terminal write attempts.
fn terminal_write_backoff(attempt: u32) -> Duration {
    Duration::from_millis(100 * (1u64 << attempt.min(4)))
}
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
    pool: std::sync::Arc<crate::db::DbPool>,
    /// Wakes the workers when a row becomes leasable. `notify_one` stores a
    /// permit, so nothing else may share it: a waiter that is not a worker
    /// (the sweep) can consume the permit and leave the due row pending until
    /// the next enqueue.
    notify: Arc<Notify>,
    /// Wakes the lease-expiry sweep; `stop` is the only producer.
    sweep_notify: Arc<Notify>,
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
#[derive(Clone)]
struct QueueWorker {
    pool: std::sync::Arc<crate::db::DbPool>,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
    handler: Arc<Handler>,
    dead_letter: Arc<DeadLetter>,
}

/// Resets rows left `in_progress` with an expired lock TTL back to `pending`
/// so they can be leased again (crash/panic recovery).
fn recover_update(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tasks SET status='pending', locked_until=0 WHERE status='in_progress' AND locked_until < ?1",
        params![now_f64()],
    )?;
    Ok(())
}

/// Base delay × 2^attempts (attempts = retries already done), capped at 300s.
/// Applied at the queue layer so the attempt count actually reaches the
/// backoff computation. The cap only ever scales *up*: a delay the server
/// asked for (Telegram `RetryAfter`) must not be shortened, retrying earlier
/// than allowed just re-triggers the flood control it came from.
fn scaled_retry_delay(base: f64, attempts: i32) -> f64 {
    (base * 2f64.powi(attempts)).min(300.0).max(base)
}

impl PersistentTaskQueue {
    /// Wraps the shared DB pool; the schema is initialized once by
    /// [`crate::db::open_store`] (all three stores share the pool).
    pub fn new(pool: std::sync::Arc<crate::db::DbPool>) -> Self {
        Self {
            pool,
            notify: Arc::new(Notify::new()),
            sweep_notify: Arc::new(Notify::new()),
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
        let mut handles = Vec::with_capacity(QUEUE_WORKERS + 1);
        for _ in 0..QUEUE_WORKERS {
            let worker = QueueWorker {
                pool: std::sync::Arc::clone(&self.pool),
                notify: Arc::clone(&self.notify),
                stop: Arc::clone(&self.stop),
                handler: Arc::clone(&handler),
                dead_letter: Arc::clone(&dead_letter),
            };
            handles.push(tokio::spawn(worker.run_loop_supervised()));
        }
        // Periodic lease-expiry sweep: recovers rows a crashed/panicked
        // worker left `in_progress` (the lock TTL bounds the wait). Its own
        // notify (not the workers'): sharing that one let this task consume a
        // `notify_one` permit meant for a worker, which then slept through a
        // due row until some later event. Only `stop` wakes it.
        let sweep_pool = std::sync::Arc::clone(&self.pool);
        let sweep_notify = Arc::clone(&self.sweep_notify);
        let sweep_stop = Arc::clone(&self.stop);
        handles.push(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                let notified = sweep_notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = interval.tick() => {}
                }
                if sweep_stop.load(Ordering::Relaxed) {
                    break;
                }
                let result = sweep_pool.with_conn(move |conn| recover_update(conn)).await;
                if let Err(e) = result {
                    log::error!("queue sweep failed: {e}");
                }
            }
        }));
        *self.worker.lock() = handles;
    }

    pub async fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
        self.sweep_notify.notify_waiters();
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
        log::debug!("enqueued {id} (run_after {run_after:.1})");
        self.pool.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES (?1, ?2, ?3, 0, 'pending', 0, ?4)",
                params![id, payload, run_after, now_f64()],
            )?;
            Ok(())
        })
        .await?;
        // `notify_one` stores a permit when no worker is registered, so a
        // notification fired between a worker's DB reads and its `notified()`
        // registration is not lost (notify_waiters would drop it). The
        // awakened worker re-leases and finds the new row.
        self.notify.notify_one();
        Ok(())
    }

    /// Pending task count and the oldest `run_after`, for the periodic sweep's
    /// health line. Deliberately separate from the worker's own
    /// `earliest_run_after`: that one runs on every idle worker cycle and must
    /// stay a single indexed `MIN`, while the count is only asked for once per
    /// sweep.
    pub async fn pending_backlog(&self) -> Option<(i64, f64)> {
        let result = self
            .pool
            .with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT COUNT(*), MIN(run_after) FROM tasks WHERE status='pending'")?;
                let mut rows = stmt.query([])?;
                match rows.next()? {
                    Some(row) => {
                        let count = row.get::<_, i64>(0)?;
                        match row.get::<_, Option<f64>>(1)? {
                            Some(oldest) if count > 0 => Ok(Some((count, oldest))),
                            _ => Ok(None),
                        }
                    }
                    None => Ok(None),
                }
            })
            .await;
        match result {
            Ok(v) => v,
            Err(e) => {
                log::error!("queue backlog query failed: {e}");
                None
            }
        }
    }

    async fn recover_stale(&self) {
        self.recover_sweep().await;
    }

    async fn recover_sweep(&self) {
        let result = self.pool.with_conn(move |conn| recover_update(conn)).await;
        if let Err(e) = result {
            log::error!("queue recovery failed: {e}");
        }
    }
}

/// Last-resort terminal state for a row whose `DELETE` would not go through:
/// `done` is invisible to `lease_next` (`status='pending'`), to the expiry
/// sweep (`status='in_progress'`) and to the backlog line, so a task that
/// already ran cannot be leased and run again.
async fn mark_done(pool: &std::sync::Arc<crate::db::DbPool>, id: &str) -> rusqlite::Result<()> {
    let id = id.to_string();
    pool.with_conn(move |conn| {
        conn.execute(
            "UPDATE tasks SET status='done', locked_until=0 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await
}

/// Which task a lease/retry/dead-letter line is about: the chat from the
/// stored payload, plus the post's normalized cache key when the payload
/// carries one (`ForwardMessages` has no source URL). Without these a queue
/// line named only a row id, which is useless to whoever reads the log — the
/// row id is assigned at insert time and appears nowhere else.
///
/// Built only when the line is actually logged (log arguments are lazy).
fn row_fields(payload: &Value) -> String {
    let chat = payload
        .get("chat_id")
        .or_else(|| payload.get("from_chat_id"))
        .and_then(Value::as_i64);
    let key = payload
        .get("source_url")
        .and_then(Value::as_str)
        .map(crate::handlers::log_key);
    match (chat, key) {
        (Some(chat), Some(key)) => format!("chat={chat} [key={key}]"),
        (Some(chat), None) => format!("chat={chat}"),
        (None, Some(key)) => format!("[key={key}]"),
        (None, None) => String::new(),
    }
}

impl QueueWorker {
    /// Supervised worker: the inner loop runs in its own task so a panic
    /// (e.g. inside a handler or a DB closure) kills only that task; the
    /// supervisor respawns it until stop is set. The row a dead worker had
    /// leased is recovered by the periodic sweep once its lock TTL expires.
    async fn run_loop_supervised(self) {
        while !self.stop.load(Ordering::Relaxed) {
            let worker = self.clone();
            if let Err(e) = tokio::spawn(async move { worker.run_loop().await }).await {
                log::error!("queue worker panicked, restarting: {e}");
            }
        }
    }

    async fn run_loop(self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.lease_next().await {
                Ok(Some(row)) => self.process(row).await,
                Ok(None) => {
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
                // A lease failure while rows are due would otherwise loop
                // with sleep(0) and hammer SQLite; back off briefly.
                Err(e) => {
                    log::error!("queue lease failed: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Leases the oldest due row (sets it `in_progress` with a lock TTL).
    /// Errors are surfaced so the caller can back off instead of spinning.
    async fn lease_next(&self) -> Result<Option<LeasedRow>, rusqlite::Error> {
        self.pool.with_conn(|conn| {
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
        .await
    }

    async fn earliest_run_after(&self) -> Option<f64> {
        let result = self
            .pool
            .with_conn(|conn| {
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

    /// Processes one leased row, keeping the lease alive while the handler
    /// runs. Without the heartbeat a task longer than [`LOCK_TTL_SECONDS`]
    /// (slow download, ugoira encode, rate-limited batch forward) would have
    /// its lease expire mid-run; the expiry sweep would flip the row back to
    /// `pending` and another worker would process it again — duplicate sends.
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
        let fields = row_fields(&payload);
        log::debug!(
            "processing {} {fields} (attempt {})",
            row.id,
            row.attempts + 1
        );
        let attempt_started = std::time::Instant::now();
        let outcome = self.run_with_lease(&row.id, payload).await;
        let attempt_ms = attempt_started.elapsed().as_millis();
        match outcome {
            Ok(()) => {
                log::debug!("task {} {fields} completed in {attempt_ms}ms", row.id);
                self.delete_row(&row.id).await;
            }
            Err(QueueError::Retryable {
                delay_seconds,
                payload,
            }) => {
                if row.attempts as u32 >= MAX_RETRIES {
                    // The queue keeps only the payload, not the last error, so
                    // the cause of an exhausted retry is just that: exhausted.
                    // (The dead-letter message is read by the user, so it must
                    // not restate its own wrapper — see `failure_text`.)
                    let message = "retries exhausted".to_string();
                    log::error!(
                        "dead-lettering {} {fields}: {message} after {} attempt(s)",
                        row.id,
                        row.attempts + 1
                    );
                    self.delete_row(&row.id).await;
                    (self.dead_letter)(payload, message).await;
                } else {
                    let delay = scaled_retry_delay(delay_seconds, row.attempts);
                    log::debug!(
                        "task {} {fields} attempt {} took {attempt_ms}ms, rescheduled in {delay:.1}s",
                        row.id,
                        row.attempts + 1
                    );
                    self.reschedule(&row.id, payload, delay, row.attempts + 1)
                        .await;
                }
            }
            Err(QueueError::Permanent { message, payload }) => {
                log::error!("dead-lettering {} {fields}: {message}", row.id);
                self.delete_row(&row.id).await;
                (self.dead_letter)(payload, message).await;
            }
        }
    }

    /// Drives the handler to completion, refreshing the row's `locked_until`
    /// every 30 s so the expiry sweep never re-leases a still-running task.
    /// The heartbeat is part of this future, not a separate spawned task: if
    /// the worker task dies (panic) the heartbeat dies with it and the sweep
    /// recovers the row exactly as before.
    async fn run_with_lease(&self, id: &str, payload: Value) -> Result<(), QueueError> {
        let fut = (self.handler)(payload);
        tokio::pin!(fut);
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // The first interval tick fires immediately; skip it (the lease was
        // just set by lease_next).
        interval.tick().await;
        let id_owned = id.to_string();
        loop {
            tokio::select! {
                result = &mut fut => return result,
                _ = interval.tick() => {
                    let now = now_f64();
                    let id = id_owned.clone();
                    let result = self
                        .pool
                        .with_conn(move |conn| {
                            conn.execute(
                                "UPDATE tasks SET locked_until=?1 WHERE id=?2 AND status='in_progress'",
                                params![now + LOCK_TTL_SECONDS, id],
                            )
                        })
                        .await;
                    if let Err(e) = result {
                        log::error!("queue lease heartbeat failed: {e}");
                    }
                }
            }
        }
    }

    /// Deletes a finished row. A failure here is not cosmetic: the row would
    /// stay `in_progress` with a live lease, the next sweep would flip it back
    /// to `pending`, and the *completed* task would run again — a second album,
    /// a second edit prompt, a second channel copy. So the delete is retried
    /// (a busy/contended DB is the usual cause and clears), and if the DB still
    /// refuses, the row is marked `done` — a status neither the lease query
    /// (`pending`) nor the sweep (`in_progress`) looks at — so a task that
    /// already ran can never be re-leased. Both writes failing is logged at
    /// error level with the row id, since that is the one case where a
    /// duplicate send stays possible.
    async fn delete_row(&self, id: &str) {
        for attempt in 0..TERMINAL_WRITE_ATTEMPTS {
            match self.try_delete_row(id).await {
                Ok(()) => return,
                Err(e) => {
                    log::error!("queue delete failed (attempt {}): {e}", attempt + 1);
                    tokio::time::sleep(terminal_write_backoff(attempt)).await;
                }
            }
        }
        match mark_done(&self.pool, id).await {
            Ok(()) => log::warn!("queue: row {id} marked done instead of deleted"),
            Err(e) => log::error!(
                "queue: row {id} could not be deleted or marked done ({e}); \
                 the expiry sweep may run this finished task again"
            ),
        }
    }

    async fn try_delete_row(&self, id: &str) -> rusqlite::Result<()> {
        let id = id.to_string();
        self.pool
            .with_conn(move |conn| {
                conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
                Ok(())
            })
            .await
    }

    /// Writes back a retryable attempt's state. A failure is retried: the row
    /// would otherwise stay `in_progress`, and the expiry sweep would re-run
    /// the attempt from its *previous* payload — re-sending batches the last
    /// attempt had already delivered. Unlike [`Self::delete_row`] there is no
    /// safe terminal fallback here (marking it done would drop the retry
    /// without telling anyone), so a persistent failure is logged loudly and
    /// the sweep's re-run — at-least-once, the documented trade — is named.
    async fn reschedule(&self, id: &str, payload: Value, delay_seconds: f64, attempts: i32) {
        let id = id.to_string();
        let payload = payload.to_string();
        let run_after = now_f64() + delay_seconds;
        let mut last_error = None;
        for attempt in 0..TERMINAL_WRITE_ATTEMPTS {
            let id = id.clone();
            let payload = payload.clone();
            let result = self
                .pool
                .with_conn(move |conn| {
                    conn.execute(
                        "UPDATE tasks SET payload=?1, run_after=?2, attempts=?3, status='pending', locked_until=0 WHERE id=?4",
                        params![payload, run_after, attempts, id],
                    )?;
                    Ok(())
                })
                .await;
            match result {
                Ok(()) => {
                    // Same permit semantics as enqueue: never lose the wakeup.
                    self.notify.notify_one();
                    return;
                }
                Err(e) => {
                    log::error!("queue reschedule failed (attempt {}): {e}", attempt + 1);
                    last_error = Some(e.to_string());
                    tokio::time::sleep(terminal_write_backoff(attempt)).await;
                }
            }
        }
        log::error!(
            "queue: row {id} could not be rescheduled ({}); the expiry sweep will \
             re-run this attempt from its previous state",
            last_error.unwrap_or_default()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn scaled_retry_delay_scales_and_caps() {
        assert_eq!(scaled_retry_delay(1.0, 0), 1.0);
        assert_eq!(scaled_retry_delay(1.0, 1), 2.0);
        assert_eq!(scaled_retry_delay(1.0, 2), 4.0);
        assert_eq!(scaled_retry_delay(1.5, 1), 3.0);
        assert_eq!(scaled_retry_delay(1.0, 10), 300.0, "capped at 300s");
        assert_eq!(scaled_retry_delay(300.0, 0), 300.0);
        // A server-asked delay above the cap is honoured, not truncated: a
        // 1800s flood-control wait used to become 300s and earn another 429.
        assert_eq!(scaled_retry_delay(1800.0, 0), 1800.0);
        assert_eq!(scaled_retry_delay(1800.0, 1), 1800.0);
    }

    async fn new_queue() -> (PersistentTaskQueue, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let pool = crate::db::open_store(path.to_str().unwrap()).unwrap();
        let queue = PersistentTaskQueue::new(pool);
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

    #[test]
    fn row_fields_name_the_chat_and_the_post() {
        // The payload shapes the three task variants store.
        assert_eq!(
            row_fields(&serde_json::json!({
                "chat_id": 111,
                "source_url": "https://x.com/u/status/1"
            })),
            "chat=111 [key=twitter:1]"
        );
        // A forward has no source URL; a chat id alone must still name the line.
        assert_eq!(
            row_fields(&serde_json::json!({"from_chat_id": 111, "to_chat_id": 222})),
            "chat=111"
        );
        // Garbage in the payload must not panic a log line.
        assert_eq!(row_fields(&serde_json::json!({"chat_id": "111"})), "");
        assert_eq!(row_fields(&serde_json::Value::Null), "");
    }

    /// The row a leaked deletion would resurrect: `done` is invisible to the
    /// lease query, so a task that already ran cannot be run again.
    #[tokio::test]
    async fn done_rows_are_never_leased() {
        let (queue, _dir) = new_queue().await;
        let runs = Arc::new(AtomicUsize::new(0));
        queue
            .enqueue(serde_json::json!({"chat_id": 1}), now_f64())
            .await
            .unwrap();
        let id: String = queue
            .pool
            .with_conn(|conn| conn.query_row("SELECT id FROM tasks", [], |r| r.get(0)))
            .await
            .unwrap();
        mark_done(&queue.pool, &id).await.unwrap();

        assert_eq!(
            queue.pending_backlog().await,
            None,
            "a done row is not pending work"
        );
        let runs_worker = runs.clone();
        queue
            .start(
                move |_payload| {
                    runs_worker.fetch_add(1, AtomicOrdering::SeqCst);
                    async { Ok(()) }
                },
                |_payload, _message| async {},
            )
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            runs.load(AtomicOrdering::SeqCst),
            0,
            "the finished row must not run again"
        );
        queue.stop().await;
    }

    #[tokio::test]
    async fn pending_backlog_counts_only_unleased_rows() {
        let (queue, _dir) = new_queue().await;
        assert_eq!(queue.pending_backlog().await, None, "empty queue");

        let due = now_f64();
        queue
            .enqueue(serde_json::json!({"chat_id": 1}), due)
            .await
            .unwrap();
        queue
            .enqueue(serde_json::json!({"chat_id": 2}), due + 600.0)
            .await
            .unwrap();
        // Hold the first row in the handler so it is leased, not pending: a
        // health line that reported work already in flight as backlog would be
        // lying about the queue.
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        queue
            .start(
                move |_payload| {
                    let held = held.clone();
                    async move {
                        held.notified().await;
                        Ok(())
                    }
                },
                |_payload, _message| async {},
            )
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            queue.pending_backlog().await.map(|(n, _)| n),
            Some(1),
            "the leased row is not pending"
        );
        let (_, oldest) = queue.pending_backlog().await.unwrap();
        assert!(
            (oldest - (due + 600.0)).abs() < 1.0,
            "oldest is the earliest run_after: {oldest}"
        );
        release.notify_one();
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
        // Insert a stale leased row directly (lease expired). open_store runs
        // the schema; the queue below shares the same pool.
        let pool = crate::db::open_store(path.to_str().unwrap()).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES ('task_stale', '{\"s\":1}', 0, 0, 'in_progress', ?1, 0)",
                params![now_f64() - 10.0],
            )
            .unwrap();
        }
        let queue = PersistentTaskQueue::new(pool);
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

    #[tokio::test]
    async fn runtime_sweep_recovers_expired_lease() {
        let (queue, _dir) = new_queue().await;
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
        // Insert a stale leased row AFTER startup: without a runtime sweep it
        // would stay `in_progress` forever (only start() used to recover).
        {
            let conn = rusqlite::Connection::open(queue.pool.path()).unwrap();
            conn.execute(
                "INSERT INTO tasks (id, payload, run_after, attempts, status, locked_until, created_at) \
                 VALUES ('task_stale_runtime', '{\"s\":1}', 0, 0, 'in_progress', ?1, 0)",
                params![now_f64() - 1000.0],
            )
            .unwrap();
        }
        queue.recover_sweep().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "expired lease must be recovered and processed exactly once"
        );
        queue.stop().await;
    }
}
