//! URL extraction and the per-URL media pipeline: bounded job channel +
//! worker pool, link-cache fast path, fetch, task build and send dispatch.

use super::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE, log_key, reply};
use crate::db::now_f64;
use crate::link_cache::{CachedMediaKind, CachedPost};
use crate::send::{self, MediaItemPayload, Task};
use crate::state::ChatData;
use std::collections::HashSet;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, Message, MessageEntityKind};
use x_media::media::Media;

/// One URL job: bot handle + the message + the extracted URL.
type UrlJob = (Bot, Message, String);
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
            while !URL_STOP.load(std::sync::atomic::Ordering::Relaxed) {
                let job = rx.lock().await.recv().await;
                match job {
                    Some((bot, message, url)) => url_media(bot, &message, &url).await,
                    None => break,
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
            let _ = handle.await;
        }
    }
}

/// Extracts URL and text-link entities (text + caption), deduped in order.
pub fn extract_urls(message: &Message) -> Vec<String> {
    let mut urls = Vec::new();
    for entity in message.parse_entities().into_iter().flatten() {
        match entity.kind() {
            MessageEntityKind::Url => urls.push(entity.text().to_string()),
            MessageEntityKind::TextLink { url } => urls.push(url.to_string()),
            _ => {}
        }
    }
    for entity in message.parse_caption_entities().into_iter().flatten() {
        match entity.kind() {
            MessageEntityKind::Url => urls.push(entity.text().to_string()),
            MessageEntityKind::TextLink { url } => urls.push(url.to_string()),
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    // Dedup by the normalized post id so variant URLs of the same post
    // (/status/1 vs /status/1/photo/1) are sent once; unsupported URLs fall
    // back to exact-string dedup.
    urls.retain(|url| seen.insert(x_media::site::cache_key(url).unwrap_or_else(|| url.clone())));
    urls
}

/// For locally produced media (encoded ugoira MP4) the thumbnail URL is a
/// hotlink-protected remote URL Telegram may not fetch; let Telegram generate
/// its own thumbnail instead.
fn thumbnail_for(media: &Media) -> Option<String> {
    let url = media.url();
    if url.starts_with("http://") || url.starts_with("https://") {
        media.thumbnail_url().map(str::to_string)
    } else {
        None
    }
}

fn media_to_payload(media: &Media, sensitive: bool) -> MediaItemPayload {
    let fallback_url = media.smaller_url().map(str::to_string);
    match media {
        // A gif inside a group becomes a video item; a lone gif takes the
        // animation path (see url_media).
        Media::Illustration { .. } => MediaItemPayload::Photo {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            fallback_url,
            file_id: false,
        },
        Media::Video { .. } => MediaItemPayload::Video {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            thumbnail: thumbnail_for(media),
            fallback_url,
            file_id: false,
        },
        Media::Animated { .. } => MediaItemPayload::Video {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            thumbnail: thumbnail_for(media),
            fallback_url,
            file_id: false,
        },
    }
}

pub(crate) async fn enqueue_retry(task: Task, delay_seconds: f64) {
    let payload = serde_json::to_value(task).expect("task serializes");
    let run_after = now_f64() + delay_seconds;
    if let Err(e) = TASK_QUEUE.enqueue(payload, run_after).await {
        log::error!("failed to enqueue retry: {e}");
    }
}

/// Sends a task and handles the outcome: post-send actions on success, retry
/// enqueue on retryable failure, reply + link-cache invalidation on
/// permanent failure (a stale cached file id must not repeat forever).
async fn dispatch_send(bot: Bot, message: &Message, task: &Task, url: &str) {
    let result = match task {
        Task::SendAnimation { .. } => send::send_animation(&bot, task).await,
        Task::SendMediaSequence { .. } => send::send_media_sequence(&bot, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    };
    match result {
        Ok(message_ids) => {
            log::info!(
                "sent {} message(s) for [key={}]",
                message_ids.len(),
                log_key(url)
            );
            send::post_send_actions(&bot, task, message_ids).await;
            // The task settled: drop any keep-alive temp media.
            send::release_keep_alive(task);
        }
        Err(send::SendError::Retryable {
            delay_seconds,
            task,
        }) => {
            log::info!(
                "send for [key={}] failed, queued for retry in {delay_seconds:.1}s",
                log_key(url)
            );
            enqueue_retry(task, delay_seconds).await;
            let _ = reply(bot, message.clone(), "Send failed. Task queued for retry.").await;
        }
        Err(send::SendError::Permanent {
            message: err_message,
            task,
        }) => {
            send::invalidate_cache(&task).await;
            send::release_keep_alive(&task);
            log::error!("send for {url} failed permanently: {err_message}");
            let _ = reply(bot, message.clone(), format!("Send failed: {err_message}")).await;
        }
    }
}

/// Builds the send task from ready-made items, sharing the payload shape
/// between the fresh-fetch and link-cache paths.
#[allow(clippy::too_many_arguments)]
fn build_send_task(
    chat_data: &ChatData,
    message: &Message,
    source_url: String,
    caption: String,
    items: Vec<MediaItemPayload>,
    cache_data: Option<CachedPost>,
) -> Task {
    let chat_id = message.chat.id.0;
    if items.len() == 1 && matches!(items[0], MediaItemPayload::Animation { .. }) {
        Task::SendAnimation {
            chat_id,
            reply_to_message_id: message.id.0 as i64,
            caption,
            animation: items.into_iter().next().unwrap(),
            source_url,
            edit_before_forward: chat_data.edit_before_forward,
            forward_channel_id: chat_data.forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(message.id.0 as i64),
            cache_data,
        }
    } else {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id: message.id.0 as i64,
            caption,
            // Photos first so a mixed photo+video group starts with a photo
            // (Telegram's sendMediaGroup rule); order within each kind is kept.
            media_batches: send::chunk_media_items(send::photos_first(items)),
            batch_index: 0,
            sent_message_ids: vec![],
            source_url,
            edit_before_forward: chat_data.edit_before_forward,
            forward_channel_id: chat_data.forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(message.id.0 as i64),
            cache_data,
        }
    }
}

async fn url_media(bot: Bot, message: &Message, url: &str) {
    let chat_id = message.chat.id.0;
    if let Err(e) = bot
        .send_chat_action(ChatId(chat_id), ChatAction::Typing)
        .await
    {
        log::error!("send_chat_action failed: {e}");
    }

    // Link cache: a post sent before is re-sent from Telegram file ids —
    // no source-site request, no download, no upload. Keyed by the
    // normalized post id so x.com / fxtwitter / /photo/N variants collide.
    if let Some(key) = x_media::site::cache_key(url)
        && let Some(cached) = LINK_CACHE.get(&key, CONFIG.link_cache_ttl).await
    {
        log::debug!("link cache hit for {key}");
        let chat_data = CHAT_STORE.get(chat_id).await;
        // Cache keys are prefixed with the site id ("twitter:…"), matching
        // the value a fresh fetch would read from Fetched::site_id.
        let site = x_media::site::site_id_from_key(&key);
        let format = chat_data
            .message_format
            .get(site)
            .cloned()
            .unwrap_or_default();
        let caption = if format.is_empty() {
            x_media::site::truncate_caption(&cached.caption)
        } else {
            x_media::site::caption_from_fields(
                &format,
                "",
                &cached.url,
                &cached.author,
                &cached.author_url,
                &cached.title,
                &cached.tags,
            )
        };
        let items: Vec<MediaItemPayload> = cached
            .media
            .iter()
            .map(|m| match m.kind {
                CachedMediaKind::Photo => MediaItemPayload::Photo {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    fallback_url: None,
                    file_id: true,
                },
                CachedMediaKind::Video => MediaItemPayload::Video {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    thumbnail: None,
                    fallback_url: None,
                    file_id: true,
                },
                CachedMediaKind::Animation => MediaItemPayload::Animation {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    file_id: true,
                },
            })
            .collect();
        let task = build_send_task(
            &chat_data,
            message,
            cached.url.clone(),
            caption,
            items,
            Some(cached),
        );
        dispatch_send(bot, message, &task, url).await;
        return;
    }

    log::debug!("fetching {url} [key={}]", log_key(url));
    match x_media::site::fetch(url).await {
        // Unsupported links are ignored silently (Python parity).
        Ok(None) => {
            log::debug!("no site pattern matches {url}; ignoring");
        }
        // Retries exhausted: notify the user (Rust-only requirement 3).
        Err(e) => {
            log::error!("fetch {url}: {e}");
            let _ = reply(
                bot,
                message.clone(),
                "Failed to fetch media from this link.",
            )
            .await;
        }
        Ok(Some(mut fetched)) => {
            if fetched.media.is_empty() {
                let _ = reply(
                    bot,
                    message.clone(),
                    "No media found or media type is not supported.",
                )
                .await;
                return;
            }
            let chat_data = CHAT_STORE.get(chat_id).await;
            // Per-site caption format override (empty -> built-in caption).
            let format = chat_data
                .message_format
                .get(fetched.site_name())
                .cloned()
                .unwrap_or_default();
            let caption = fetched.caption_with(&format);
            // Raw render data for the link cache; the send fills in the
            // Telegram file ids and persists the entry.
            let cache_data = fetched
                .render_fields()
                .map(|(author, author_url, title, tags)| CachedPost {
                    url: fetched.source_url.clone(),
                    caption: fetched.caption.clone(),
                    title: title.to_string(),
                    author: author.to_string(),
                    author_url: author_url.to_string(),
                    tags: tags.to_string(),
                    sensitive: fetched.sensitive,
                    media: vec![],
                });
            let items: Vec<MediaItemPayload> = fetched
                .media
                .iter()
                .map(|media| media_to_payload(media, fetched.sensitive))
                .collect();
            let task = build_send_task(
                &chat_data,
                message,
                fetched.source_url.clone(),
                caption,
                items,
                cache_data,
            );
            // Hand the keep-alive temp dir (ugoira / bsky remux MP4) to the
            // retry registry: a queued retry runs after this function returns
            // and the fetch's own TempDir is dropped, so without this the
            // local file would be gone by the time the retry sends it.
            if let Some(dir) = fetched.take_keep_alive() {
                send::KEEP_ALIVE.lock().push(dir);
            }
            dispatch_send(bot, message, &task, url).await;
        }
    }
}
