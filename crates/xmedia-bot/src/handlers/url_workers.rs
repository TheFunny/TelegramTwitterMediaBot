//! The URL job channel and its worker pool: a bounded queue (backpressure
//! instead of unbounded spawns) drained by [`URL_WORKERS`] supervised workers.
//!
//! teloxide's per-chat workers are sequential, so a batch forward needs its own
//! concurrency: this is where a link handed over by `handlers::mod` actually
//! reaches the pipeline.

use super::urls::{PostSend, url_media};
use crate::ctx::CONTEXT;
use std::sync::LazyLock;
use teloxide::types::Message;

/// One URL job: the message + the extracted URL (the sender and stores come
/// from the shared [`AppContext`], assembled from statics inside the worker).
type UrlJob = (Message, String);
/// Bounded channel of URL jobs drained by [`start_url_workers`]. The bound
/// caps both queued memory and shutdown backlog; a full channel applies
/// backpressure to the per-chat handler instead of spawning unbounded tasks.
pub(crate) static URL_JOBS: LazyLock<
    parking_lot::Mutex<Option<tokio::sync::mpsc::Sender<UrlJob>>>,
> = LazyLock::new(|| parking_lot::Mutex::new(None));
/// Set by main's shutdown sequence; workers stop pulling new jobs.
pub(crate) static URL_STOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// JoinHandles of the URL workers, awaited by [`stop_url_workers`].
static URL_WORKER_HANDLES: LazyLock<parking_lot::Mutex<Option<Vec<tokio::task::JoinHandle<()>>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));

/// Worker count draining URL jobs; keeps the old 8-permit concurrency cap
/// while bounding how many jobs can be queued at all.
const URL_WORKERS: usize = 8;

/// Starts the URL job workers (called once from main after the queue starts).
/// teloxide dispatches updates to a per-chat worker that handles them
/// sequentially, so a batch-forward of many messages would otherwise be
/// processed one at a time (fetch + send each, roughly a second per
/// message); the workers add throughput, and FIFO order preserves per-message
/// URL order.
pub async fn start_url_workers() {
    let (tx, rx) = tokio::sync::mpsc::channel::<UrlJob>(256);
    *URL_JOBS.lock() = Some(tx);
    let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
    let mut handles = Vec::with_capacity(URL_WORKERS);
    for _ in 0..URL_WORKERS {
        let rx = std::sync::Arc::clone(&rx);
        handles.push(tokio::spawn(async move {
            // Supervised like the queue workers: a panic inside a worker
            // (a handler, a poisoned lock) used to kill it for good and
            // silently shrink the pool — the remaining workers keep the
            // channel drained, so nothing else surfaces the loss. The job the
            // panicking worker held is lost; the panic is not.
            while !URL_STOP.load(std::sync::atomic::Ordering::Relaxed) {
                let rx = std::sync::Arc::clone(&rx);
                if let Err(e) = tokio::spawn(async move {
                    while !URL_STOP.load(std::sync::atomic::Ordering::Relaxed) {
                        let job = rx.lock().await.recv().await;
                        match job {
                            Some((message, url)) => {
                                url_media(
                                    &CONTEXT,
                                    message.chat.id.0,
                                    message.id.0 as i64,
                                    &url,
                                    PostSend::FromChat,
                                )
                                .await;
                            }
                            None => break,
                        }
                    }
                })
                .await
                {
                    log::error!("url worker panicked, restarting: {e}");
                }
            }
        }));
    }
    *URL_WORKER_HANDLES.lock() = Some(handles);
}

/// Stops the URL workers: sets the stop flag, drops the job channel (so
/// workers blocked in \`recv()\` wake with \`None\` and exit) and awaits the
/// worker tasks. Each worker finishes its in-flight job first; jobs still
/// queued in the channel are abandoned (the old implementation neither
/// drained them nor woke blocked workers — it only set a flag checked
/// between jobs).
pub async fn stop_url_workers() {
    URL_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    // Dropping the sender makes every worker's recv() return None.
    *URL_JOBS.lock() = None;
    // Take the handles first so the lock guard drops before the awaits.
    let handles = URL_WORKER_HANDLES.lock().take();
    if let Some(handles) = handles {
        for handle in handles {
            if let Err(e) = handle.await {
                log::error!("url worker panicked at shutdown: {e}");
            }
        }
    }
}
