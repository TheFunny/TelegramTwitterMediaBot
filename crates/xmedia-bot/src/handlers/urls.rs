//! URL extraction and the per-URL media pipeline: bounded job channel +
//! worker pool, link-cache fast path, fetch, task build and send dispatch.

use super::{log_key, reply};
use crate::ctx::{AppContext, CONTEXT};
use crate::link_cache::{CachedMedia, CachedMediaKind, CachedPost};
use crate::media_sender::MediaSender;
use crate::send::{self, Delivery, MediaItemPayload, Task};
use crate::state::ChatData;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::LazyLock;
use teloxide::RequestError;
use teloxide::types::{ChatAction, ChatId, Message, MessageEntityKind, MessageId};
use x_media::media::Media;

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

/// One fetch per post at a time, keyed by the normalized cache key. Two chats
/// posting the same link at the same moment (or a batch forward and a queued
/// retry) used to run two full fetches: two sets of source requests, and for an
/// ugoira or a bsky video two ffmpeg encodes of the same post. The first caller
/// runs it and the rest wait for its result. The entry is dropped the moment
/// the fetch settles, so this dedupes what is *concurrent* and never answers
/// from an old result: a repeat later fetches again, and a failure is not
/// cached (the user may well retry it).
static IN_FLIGHT_FETCHES: LazyLock<parking_lot::Mutex<HashMap<String, SharedFetch<FetchOutcome>>>> =
    LazyLock::new(Default::default);

/// A fetched post (or the error that stopped it), shared as-is: the error side
/// is not `Clone`, so callers read it through the `Arc` — the same shape the
/// send paths use for `&Fetched`.
type FetchOutcome = Result<Option<x_media::site::Fetched>, x_media::site::FetchError>;

/// The channel a sharer publishes its result on, and waiters subscribe to.
type SharedFetch<T> = tokio::sync::broadcast::Sender<std::sync::Arc<T>>;

/// Fetches `url`, sharing one in-flight fetch per `key` (its site cache key)
/// with every other caller asking for the same post meanwhile.
async fn fetch_shared(key: &str, url: &str) -> std::sync::Arc<FetchOutcome> {
    shared_fetch(&IN_FLIGHT_FETCHES, key, || x_media::site::fetch(url)).await
}

/// [`fetch_shared`]'s core, over the caller's own map so the sharing rules can
/// be tested without a network fetch.
///
/// A caller that finds a live entry subscribes to it and waits; the caller that
/// created the entry runs `fetch` and publishes the result. Two things keep
/// that from stranding a request: the entry is removed by a guard (so a
/// cancelled fetch cannot leave waiters subscribed to a channel nothing will
/// ever write to), and a waiter whose sharer vanished fetches for itself.
async fn shared_fetch<T, F, Fut>(
    map: &parking_lot::Mutex<HashMap<String, SharedFetch<T>>>,
    key: &str,
    fetch: F,
) -> std::sync::Arc<T>
where
    T: Send + Sync + 'static,
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let (sender, leader) = {
        let mut map = map.lock();
        match map.get(key) {
            Some(sender) => (sender.clone(), false),
            None => {
                let (sender, _) = tokio::sync::broadcast::channel(1);
                map.insert(key.to_string(), sender.clone());
                (sender, true)
            }
        }
    };
    if !leader {
        // `Err` means the entry is gone without a value: the sharer was
        // cancelled, or it finished just as this caller subscribed (the
        // message predates the subscription). Fetch for ourselves instead of
        // failing a link that is perfectly fetchable.
        let mut receiver = sender.subscribe();
        // The sender clone taken from the map is dropped first: held, it would
        // keep the channel open past the sharer's exit (a broadcast channel
        // closes when *all* senders are gone), and `recv` would wait forever
        // instead of reporting that the sharer vanished.
        drop(sender);
        match receiver.recv().await {
            Ok(shared) => return shared,
            Err(_) => return std::sync::Arc::new(fetch().await),
        }
    }
    // Removes the entry on every exit path, cancellation included.
    let _guard = InFlightFetch { map, key };
    let outcome = std::sync::Arc::new(fetch().await);
    // No receiver is the common case, not an error: a lone caller has nobody
    // to publish to.
    let _ = sender.send(std::sync::Arc::clone(&outcome));
    outcome
}

/// Drops the in-flight entry it was created for, however the fetch ends.
struct InFlightFetch<'a, T> {
    map: &'a parking_lot::Mutex<HashMap<String, SharedFetch<T>>>,
    key: &'a str,
}

impl<T> Drop for InFlightFetch<'_, T> {
    fn drop(&mut self) {
        self.map.lock().remove(self.key);
    }
}

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

/// Extracts URL and text-link entities (text + caption), deduped in order.
///
/// The offset work (`parse_entities` turning entities into slices of the
/// message text) is teloxide's; the two decisions that are ours are
/// [`url_of`] and [`dedupe_urls`], which is why they are separate and tested.
pub fn extract_urls(message: &Message) -> Vec<String> {
    let entities = message
        .parse_entities()
        .into_iter()
        .flatten()
        .chain(message.parse_caption_entities().into_iter().flatten());
    dedupe_urls(
        entities
            .filter_map(|entity| url_of(entity.kind(), entity.text()))
            .collect(),
    )
}

/// The URL an entity carries: a bare `Url` entity is its own text, a
/// `TextLink` is its target (its display text is often a different string).
/// Every other entity kind (bold, code, hashtag, …) carries none.
fn url_of(kind: &MessageEntityKind, text: &str) -> Option<String> {
    match kind {
        MessageEntityKind::Url => Some(text.to_string()),
        MessageEntityKind::TextLink { url } => Some(url.to_string()),
        _ => None,
    }
}

/// Keeps the first occurrence of each link, in order. Dedup is by the
/// normalized post id, so variant URLs of the same post (`/status/1` vs
/// `/status/1/photo/1`, or a text link whose target equals a pasted URL) are
/// sent once; URLs no site claims (and plain text that is no URL) fall back to
/// exact-string dedup.
fn dedupe_urls(urls: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(urls.len());
    for url in urls {
        let key = x_media::site::cache_key(&url).unwrap_or_else(|| url.clone());
        if seen.insert(key) {
            out.push(url);
        }
    }
    out
}

/// For locally produced media (encoded ugoira MP4) the thumbnail URL is a
/// hotlink-protected remote URL Telegram may not fetch; let Telegram generate
/// its own thumbnail instead.
fn thumbnail_for(media: &Media) -> Option<String> {
    let url = media.url();
    if url.starts_with("http://") || url.starts_with("https://") {
        // An empty thumbnail string (misskey video/gif files without a
        // thumbnailUrl) must not reach Telegram; let it generate its own.
        media
            .thumbnail_url()
            .map(str::to_string)
            .filter(|t| !t.is_empty())
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

/// Sends a task and handles the outcome: post-send actions on success, retry
/// enqueue on retryable failure, reply + link-cache invalidation on
/// permanent failure (a stale cached file id must not repeat forever).
async fn dispatch_send(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to: MessageId,
    task: &Task,
    url: &str,
    started: std::time::Instant,
) {
    let result = match task {
        Task::SendAnimation { .. } => send::send_animation(ctx, task).await,
        Task::SendMediaSequence { .. } => send::send_media_sequence(ctx, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    };
    // Fetch + cache lookup + upload: the whole wait the user sat through.
    let ms = started.elapsed().as_millis();
    match result {
        Ok(message_ids) => {
            log::info!(
                "sent {} message(s) for [key={}] chat={chat_id} in {ms}ms",
                message_ids.len(),
                log_key(url)
            );
            send::post_send_actions(ctx, task, message_ids).await;
            send::settle_task(ctx, task, send::Settled::Sent).await;
        }
        Err(send::SendError::Retryable {
            delay_seconds,
            task,
        }) => {
            log::info!(
                "send for [key={}] chat={chat_id} failed after {ms}ms, queued for retry in {delay_seconds:.1}s",
                log_key(url)
            );
            // Name the post and the wait: "queued for retry" alone left the
            // user guessing which link it was and how long the wait is. The
            // promise is made only when the retry was really persisted — an
            // enqueue that failed (DB write) would leave the user waiting for
            // a retry nothing can deliver.
            let promised = if send::enqueue_retry(ctx.task_queue, &task, delay_seconds).await {
                format!(
                    "Send failed for {} — retrying in {delay_seconds:.0}s.",
                    log_key(url)
                )
            } else {
                format!(
                    "Send failed for {} and the retry could not be queued — please send the link again.",
                    log_key(url)
                )
            };
            let _ = reply(ctx.sender, chat_id, reply_to, promised).await;
        }
        Err(send::SendError::Permanent {
            message: err_message,
            task,
        }) => {
            send::settle_task(ctx, &task, send::Settled::Failed).await;
            log::error!(
                "send for [key={}] chat={chat_id} failed permanently after {ms}ms: {err_message}",
                log_key(url)
            );
            let _ = reply(
                ctx.sender,
                chat_id,
                reply_to,
                format!("Send failed: {err_message}"),
            )
            .await;
        }
    }
}

/// A cached entry's item as a send payload: the Telegram file id when the entry
/// still has one, otherwise the source URL.
///
/// The URL case is a *degraded* entry — `send::post_send`'s `invalidate_cache`
/// drops the file ids of an entry whose cached send failed permanently, keeping
/// the URLs, because a stale file id says nothing about the media. Sending from
/// the URL costs Telegram a fetch (or the upload fallback a download) and saves
/// the whole source round trip, including a ugoira encode or an HLS remux.
fn cached_media_payload(media: &CachedMedia, sensitive: bool) -> MediaItemPayload {
    let has_file_id = !media.file_id.is_empty();
    let source = if has_file_id {
        media.file_id.clone()
    } else {
        media.url.clone()
    };
    // A degraded item carries no smaller variant: a fresh fetch's would, but
    // the item is what the source itself sent, so an oversize is handled by the
    // upload fallback rather than by a URL that was never recorded.
    let fallback_url = None;
    match media.kind {
        CachedMediaKind::Photo => MediaItemPayload::Photo {
            media: source,
            has_spoiler: sensitive,
            fallback_url,
            file_id: has_file_id,
        },
        CachedMediaKind::Video => MediaItemPayload::Video {
            media: source,
            has_spoiler: sensitive,
            thumbnail: None,
            fallback_url,
            file_id: has_file_id,
        },
        CachedMediaKind::Animation => MediaItemPayload::Animation {
            media: source,
            has_spoiler: sensitive,
            file_id: has_file_id,
        },
    }
}

/// Whether a send also runs the chat's post-send actions. `/test` sends with
/// them suppressed so a test can never forward to the channel or open the
/// edit-before-forward prompt; a normal link uses whatever the chat is
/// configured with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PostSend {
    /// Apply the chat's `forward_channel_id` / `edit_before_forward`.
    FromChat,
    /// Send only: no channel forward, no edit prompt.
    Suppressed,
}

/// Builds the send task from ready-made items, sharing the payload shape
/// between the fresh-fetch, link-cache and `/test` paths.
#[allow(clippy::too_many_arguments)]
fn build_send_task(
    chat_data: &ChatData,
    chat_id: i64,
    reply_to_message_id: i64,
    source_url: String,
    caption: String,
    items: Vec<MediaItemPayload>,
    cache_data: Option<CachedPost>,
    post_send: PostSend,
) -> Task {
    // Notification ids stay set in both modes: a queued retry that
    // dead-letters should still tell the chat.
    let (edit_before_forward, forward_channel_id) = match post_send {
        PostSend::FromChat => (chat_data.edit_before_forward, chat_data.forward_channel_id),
        PostSend::Suppressed => (false, None),
    };
    Task::from_items(
        Delivery {
            chat_id,
            reply_to_message_id,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(reply_to_message_id),
        },
        source_url,
        caption,
        items,
        cache_data,
    )
}

/// The per-URL pipeline: link cache → fetch → build → send → post-send.
///
/// `post_send` selects whether the chat's forward/edit settings apply: the URL
/// workers pass [`PostSend::FromChat`], the `/test` command
/// [`PostSend::Suppressed`]. Everything else (cache write, retry enqueue,
/// dead-letter notification) is identical.
///
/// Wraps [`url_media_inner`] with the chat-action keep-alive: Telegram expires
/// an action indicator after ~5s, while a fetch (ugoira encode, HLS remux) plus
/// a download-and-reupload fallback routinely takes longer — without the
/// refresh the chat shows nothing and the bot reads as stalled.
pub(crate) async fn url_media(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to_message_id: i64,
    url: &str,
    post_send: PostSend,
) {
    // Shared with the pipeline: once the media types are known the indicator
    // switches from "typing" to "sending photo/video".
    let hint = parking_lot::Mutex::new(ActionHint::Typing);
    run_with_chat_action(
        ctx.sender,
        chat_id,
        &hint,
        url_media_inner(ctx, chat_id, reply_to_message_id, url, post_send, &hint),
    )
    .await;
}

/// Runs `pipeline` while keeping the chat's action indicator alive: Telegram
/// expires an action after ~5s, while a fetch (ugoira encode, HLS remux) plus a
/// download-and-reupload fallback routinely takes longer. The pipeline updates
/// `hint` when it knows what it is sending.
///
/// No action is ever awaited *ahead* of the pipeline: doing that held the loop
/// — and with it the fetch the user is waiting for — for a Telegram round trip,
/// once before the pipeline was polled at all and again every
/// [`ACTION_REFRESH`]. The in-flight send is held and polled *beside* the
/// pipeline instead: the opening indicator still goes out before the pipeline's
/// own first call (that is what it is for), but a slow API can no longer delay
/// anything but the next indicator.
async fn run_with_chat_action<F: Future<Output = ()>>(
    sender: &dyn MediaSender,
    chat_id: i64,
    hint: &parking_lot::Mutex<ActionHint>,
    pipeline: F,
) {
    let warn = |e: RequestError| {
        // Cosmetic indicator: a failure degrades the experience, it does not
        // break the send (a group where the bot cannot send actions).
        log::warn!("send_chat_action failed for chat {chat_id}: {e}");
    };
    // The guard is released before the await: a parking_lot guard held across
    // it makes the future !Send, and the URL workers spawn these.
    let mut action = Some(sender.send_chat_action(ChatId(chat_id), hint.lock().action()));
    tokio::pin!(pipeline);
    loop {
        tokio::select! {
            // `biased` fixes the order below: the indicator is polled ahead of
            // the pipeline, and a finished pipeline returns without arming the
            // refresh timer (no stray actions).
            biased;
            // `select!` evaluates every branch's future expression eagerly, so
            // the `None` case is an inert block: the guard is what keeps it
            // from being polled (and from unwrapping a `None`).
            result = async { action.as_mut().unwrap().await }, if action.is_some() => {
                action = None;
                if let Err(e) = result {
                    warn(e);
                }
            }
            () = &mut pipeline => return,
            () = tokio::time::sleep(ACTION_REFRESH) => {
                // One action in flight at a time: re-arming while the previous
                // send is still unanswered would drop it mid-request.
                if action.is_none() {
                    action = Some(sender.send_chat_action(ChatId(chat_id), hint.lock().action()));
                }
            }
        }
    }
}

/// How often the chat-action indicator is refreshed while a pipeline runs.
/// Telegram's indicator lasts ~5s; refreshing slightly inside that keeps it
/// on-screen continuously.
const ACTION_REFRESH: std::time::Duration = std::time::Duration::from_secs(4);

/// What the chat action should say. Unknown before the fetch, so the pipeline
/// starts with `Typing` and switches as soon as the media types are known.
#[derive(Clone, Copy)]
enum ActionHint {
    Typing,
    Photo,
    Video,
}

impl ActionHint {
    /// Photos make Telegram label the send "sending photo"; video/animation
    /// only payloads get "sending video". A mixed post takes the photo label
    /// (the group's first item is always a photo, see `photos_first`).
    fn for_items(items: &[MediaItemPayload]) -> Self {
        if items
            .iter()
            .any(|item| matches!(item, MediaItemPayload::Photo { .. }))
        {
            Self::Photo
        } else {
            Self::Video
        }
    }

    fn action(self) -> ChatAction {
        match self {
            Self::Typing => ChatAction::Typing,
            Self::Photo => ChatAction::UploadPhoto,
            Self::Video => ChatAction::UploadVideo,
        }
    }
}

/// User-facing text for a failed fetch. The [`FetchError`] class is what tells
/// the user whether the post is gone, withheld or the source is refusing
/// requests; a single generic sentence threw that away.
fn fetch_error_message(err: &x_media::site::FetchError) -> String {
    use x_media::site::FetchError;
    match err {
        FetchError::NotFound => "Post not found (deleted, private or unavailable).".to_string(),
        FetchError::Sensitive => concat!(
            "This post's media is withheld (age-restricted). ",
            "The bot owner must set TWITTER_AUTH_TOKEN to fetch it."
        )
        .to_string(),
        FetchError::Blocked => {
            "The source site refused the request (risk control). Try again later.".to_string()
        }
        FetchError::Disabled { site } => {
            format!("{} support is disabled on this bot.", site_title(site))
        }
        FetchError::Transient(_) | FetchError::Http(_) => {
            "The source site is unavailable right now (tried 3 times). Try again later.".to_string()
        }
        FetchError::MediaPrep(_) => concat!(
            "Could not prepare this post's media (its download or encode failed). ",
            "Try again later."
        )
        .to_string(),
        // Parse/shape surprises, pixiv auth details, oversized media: nothing
        // actionable for the user beyond "this did not work".
        _ => "Failed to fetch media from this link.".to_string(),
    }
}

/// Site ids are lowercase ASCII (`pixiv`); user-facing text capitalizes the
/// first letter.
fn site_title(site: &str) -> String {
    let mut chars = site.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn url_media_inner(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to_message_id: i64,
    url: &str,
    post_send: PostSend,
    hint: &parking_lot::Mutex<ActionHint>,
) {
    let reply_to = MessageId(reply_to_message_id as i32);
    // Whole-link timer for the result lines: fetch (ugoira encode, HLS remux
    // included) + cache lookup + upload — the wait the user actually had.
    let started = std::time::Instant::now();

    // Link cache: a post sent before is re-sent from Telegram file ids —
    // no source-site request, no download, no upload. Keyed by the
    // normalized post id so x.com / fxtwitter / /photo/N variants collide.
    if let Some(key) = x_media::site::cache_key(url)
        && let Some(cached) = ctx.link_cache.get(&key, ctx.config.link_cache_ttl).await
    {
        log::debug!("link cache hit for {key}");
        let chat_data = ctx.chat_store.get(chat_id).await;
        // Cache keys are prefixed with the site id ("twitter:…"), matching
        // the value a fresh fetch would read from Fetched::site_id.
        let site = x_media::site::site_id_from_key(&key);
        let format = chat_data
            .message_format
            .get(site)
            .cloned()
            .unwrap_or_default();
        // One call for both: `caption_from_fields` returns the truncated
        // built-in caption itself when the chat has no format for this site.
        let caption = x_media::site::caption_from_fields(
            &format,
            &cached.caption,
            &cached.url,
            &cached.author,
            &cached.author_url,
            &cached.title,
            &cached.content,
            &cached.tags,
        );
        let items: Vec<MediaItemPayload> = cached
            .media
            .iter()
            .map(|m| cached_media_payload(m, cached.sensitive))
            .collect();
        // The indicator switches to "sending photo/video" once the kinds are
        // known; `items` is moved into the task below.
        *hint.lock() = ActionHint::for_items(&items);
        let task = build_send_task(
            &chat_data,
            chat_id,
            reply_to_message_id,
            cached.url.clone(),
            caption,
            items,
            Some(cached),
            post_send,
        );
        dispatch_send(ctx, chat_id, reply_to, &task, url, started).await;
        return;
    }

    log::debug!("fetching [key={}]", log_key(url));
    log::trace!("fetching {url}");
    // One fetch per post at a time: a concurrent duplicate of this link waits
    // for *this* fetch instead of running its own (see [`fetch_shared`]).
    let outcome = match x_media::site::cache_key(url) {
        Some(key) => fetch_shared(&key, url).await,
        // A URL no site claims (reached only through `/test`): nothing to key
        // the sharing on, and the dispatcher answers without a request.
        None => std::sync::Arc::new(x_media::site::fetch(url).await),
    };
    match &*outcome {
        // Unsupported links are ignored silently (Python parity).
        Ok(None) => {
            // The URL itself is user data, so only `trace` names the link;
            // `debug` just records that the message was looked at.
            log::debug!("no site pattern matches the link; ignoring");
            log::trace!("no site pattern matches {url}");
        }
        // Retries exhausted: notify the user (Rust-only requirement 3).
        Err(e) => {
            log::error!("fetch [key={}]: {e}", log_key(url));
            let _ = reply(ctx.sender, chat_id, reply_to, fetch_error_message(e)).await;
        }
        Ok(Some(fetched)) => {
            if fetched.media.is_empty() {
                let _ = reply(
                    ctx.sender,
                    chat_id,
                    reply_to,
                    "No media found or media type is not supported.",
                )
                .await;
                return;
            }
            let chat_data = ctx.chat_store.get(chat_id).await;
            // Per-site caption format override (empty -> built-in caption).
            let format = chat_data
                .message_format
                .get(fetched.site_id)
                .cloned()
                .unwrap_or_default();
            let caption = fetched.caption_with(&format);
            // Raw render data for the link cache; the send fills in the
            // Telegram file ids and persists the entry.
            let cache_data =
                fetched
                    .render_fields()
                    .map(|(author, author_url, title, content, tags)| CachedPost {
                        url: fetched.source_url.clone(),
                        caption: fetched.caption.clone(),
                        title: title.to_string(),
                        content: content.to_string(),
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
            // The indicator switches to "sending photo/video" once the kinds
            // are known; `items` is moved into the task below.
            *hint.lock() = ActionHint::for_items(&items);
            let task = build_send_task(
                &chat_data,
                chat_id,
                reply_to_message_id,
                fetched.source_url.clone(),
                caption,
                items,
                cache_data,
                post_send,
            );
            // Hand the keep-alive temp dir (ugoira / bsky remux MP4) to the
            // retry registry: a queued retry runs after this function returns
            // and the fetch's own TempDir is dropped, so without this the
            // local file would be gone by the time the retry sends it. Shared
            // (Arc), so a second send of the same post holds its own reference.
            if let Some(dir) = fetched.keep_alive() {
                send::KEEP_ALIVE.lock().push(dir);
            }
            dispatch_send(ctx, chat_id, reply_to, &task, url, started).await;
        }
    }
}

// ── Startup repair: queued retries whose local media did not survive ───────

/// A post's fresh media plus the caption and cache snapshot that go with them:
/// what [`refetch`] hands [`apply_refresh`]. Plain data, so the rewrite below
/// can be tested without a network fetch (which cannot be faked here:
/// [`x_media::site::Fetched`] keeps a private field and is not constructible
/// outside its crate).
struct Refetched {
    caption: String,
    items: Vec<MediaItemPayload>,
    cache_data: Option<CachedPost>,
}

/// Whether a queued task should have its post re-fetched, because it still
/// wants a local file (ugoira MP4, a bsky remux, a downloaded temp file) that is
/// gone. Those files live in the system temp dir and the registry that keeps
/// them alive for the retry (`send::KEEP_ALIVE`) is in memory, so a restart
/// takes all of them — a retry that needs one can only dead-letter.
///
/// A partially delivered album is left alone: its remaining batches cannot be
/// reconciled with a fresh media list without risking a second copy of what the
/// user already received.
fn needs_refetch(task: &Task) -> bool {
    if let Task::SendMediaSequence {
        batch_index,
        sent_message_ids,
        ..
    } = task
        && (*batch_index > 0 || !sent_message_ids.is_empty())
    {
        return false;
    }
    task.local_media_paths().iter().any(|path| !path.exists())
}

/// Rebuilds the task from the fresh media, keeping its delivery envelope (chat,
/// reply, forward/edit settings, notify targets): the retry that was queued must
/// still deliver the same way, whoever asked for it.
fn apply_refresh(task: &Task, fresh: &Refetched) -> Option<Task> {
    let chat_id = task.chat_id()?;
    let (edit_before_forward, forward_channel_id) = match task {
        Task::SendMediaSequence {
            edit_before_forward,
            forward_channel_id,
            ..
        }
        | Task::SendAnimation {
            edit_before_forward,
            forward_channel_id,
            ..
        } => (*edit_before_forward, *forward_channel_id),
        Task::ForwardMessages { .. } => return None,
    };
    let reply_to_message_id = match task {
        Task::SendMediaSequence {
            reply_to_message_id,
            ..
        }
        | Task::SendAnimation {
            reply_to_message_id,
            ..
        } => *reply_to_message_id,
        Task::ForwardMessages { .. } => return None,
    };
    let (notify_chat_id, notify_message_id) = task.notify_target();
    Some(Task::from_items(
        Delivery {
            chat_id,
            reply_to_message_id,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
        },
        task.source_url()?.to_string(),
        fresh.caption.clone(),
        fresh.items.clone(),
        fresh.cache_data.clone(),
    ))
}

/// Fetches the post again and maps it into [`Refetched`]: the same mapping the
/// fresh-fetch path uses (per-site caption format from the chat, render fields
/// for the link-cache snapshot), so a repaired task looks like a first send.
async fn refetch(
    ctx: &AppContext<'_>,
    chat_id: i64,
    url: &str,
) -> Result<Option<Refetched>, x_media::site::FetchError> {
    let Some(fetched) = x_media::site::fetch(url).await? else {
        return Ok(None);
    };
    if fetched.media.is_empty() {
        return Ok(None);
    }
    let chat_data = ctx.chat_store.get(chat_id).await;
    let format = chat_data
        .message_format
        .get(fetched.site_id)
        .cloned()
        .unwrap_or_default();
    let caption = fetched.caption_with(&format);
    let cache_data = fetched
        .render_fields()
        .map(|(author, author_url, title, content, tags)| CachedPost {
            url: fetched.source_url.clone(),
            caption: fetched.caption.clone(),
            title: title.to_string(),
            content: content.to_string(),
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
    // The re-fetch may produce a fresh local file (ugoira / bsky remux): hand it
    // to the same keep-alive registry the first fetch uses.
    if let Some(dir) = fetched.keep_alive() {
        send::KEEP_ALIVE.lock().push(dir);
    }
    Ok(Some(Refetched {
        caption,
        items,
        cache_data,
    }))
}

/// Re-fetches every queued task whose local media did not survive the restart,
/// so the user's link is still delivered instead of dead-lettering on a file
/// that cannot come back. Returns how many rows were rewritten.
///
/// Startup only, before the queue workers start: no worker can lease a row while
/// this writes, which is what lets it replace payloads without the lease-token
/// guard every worker write-back carries.
pub(crate) async fn repair_lost_local_media(ctx: &AppContext<'_>) -> usize {
    let mut repaired = 0;
    for (id, payload) in ctx.task_queue.runnable_rows().await {
        let Ok(task) = serde_json::from_str::<Task>(&payload) else {
            continue;
        };
        if !needs_refetch(&task) {
            continue;
        }
        let (Some(url), Some(chat_id)) = (task.source_url().map(str::to_string), task.chat_id())
        else {
            continue;
        };
        match refetch(ctx, chat_id, &url).await {
            Ok(Some(fresh)) => {
                let Some(updated) = apply_refresh(&task, &fresh) else {
                    continue;
                };
                let updated = serde_json::to_value(&updated).expect("task serializes");
                if ctx.task_queue.replace_payload(&id, &updated).await {
                    repaired += 1;
                    log::info!(
                        "startup repair: re-fetched [key={}] for chat={chat_id} (its local media did not survive the restart)",
                        log_key(&url)
                    );
                }
            }
            // The post is gone or withheld now: the retry could not have
            // delivered anything either, so say why instead of letting it
            // dead-letter on a missing file.
            Ok(None) | Err(_) => {
                let (notify_chat_id, notify_message_id) = task.notify_target();
                log::warn!(
                    "startup repair: [key={}] for chat={chat_id} needed a re-fetch and none was possible",
                    log_key(&url)
                );
                send::notify_failure(
                    ctx.sender,
                    notify_chat_id,
                    notify_message_id,
                    &format!(
                        "{} — the media held for retry was lost when the bot restarted and the post could not be fetched again. Please send the link again.",
                        log_key(&url)
                    ),
                )
                .await;
            }
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::{TestStores, api_error, cached_photo, photo_item};
    use crate::media_sender::test_support::{MockSender, Outcome};
    use std::time::Duration;
    use teloxide::RequestError;

    /// The API error a caption edit that changes nothing answers with — what
    /// the mocks script for a permanent send failure. A `fn` pointer, so it
    /// can be handed to `MockSender::scripted` as-is.
    fn permanent_error() -> RequestError {
        api_error("Bad Request: message is not modified")
    }

    #[tokio::test]
    async fn cache_hit_sends_file_ids_and_degrades_on_permanent_failure() {
        let stores = TestStores::new();
        let sender = MockSender::scripted(
            vec![Outcome::GroupErr, Outcome::MessageErr],
            permanent_error,
        );
        let ctx = stores.ctx(&sender);
        stores.link_cache().put("twitter:1", &cached_photo()).await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

        // The cached file id went out as a group send; the permanent failure
        // then triggered the fire-and-forget reply (its mock error is fine).
        assert_eq!(
            sender.calls(),
            vec!["send_chat_action", "send_media_group", "send_message"]
        );
        // The entry is degraded, not dropped: the next request re-sends from
        // the source URL without a fetch.
        let entry = stores
            .link_cache()
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .expect("a stale file id must not cost the whole entry");
        assert!(entry.media[0].file_id.is_empty());
        assert_eq!(entry.media[0].url, "https://pbs.twimg.com/media/photo.jpg");
    }

    /// A degraded entry (see the test above) sends the source URL: no fetch,
    /// no download of the post's data, and no upload of our own — Telegram
    /// fetches the media it is pointed at. The absence of a `send_message`
    /// (which the fetch-error path would emit) is what proves no fetch ran.
    #[tokio::test]
    async fn a_degraded_entry_sends_by_url_without_fetching() {
        let stores = TestStores::new();
        let sender = MockSender::scripted(vec![Outcome::GroupOk], permanent_error);
        let ctx = stores.ctx(&sender);
        let mut cached = cached_photo();
        cached.media[0].file_id.clear();
        stores.link_cache().put("twitter:1", &cached).await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

        assert_eq!(sender.calls(), vec!["send_chat_action", "send_media_group"]);
        // Still cached, still degraded: a degraded entry keeps serving.
        let entry = stores
            .link_cache()
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .expect("the entry must stay");
        assert!(entry.media[0].file_id.is_empty());
    }

    /// The two payload shapes a cached item can take, asserted directly: the
    /// file id when there is one, the source URL when the entry was degraded.
    #[test]
    fn cached_media_payload_prefers_the_file_id_over_the_url() {
        let with_id = CachedMedia {
            kind: CachedMediaKind::Photo,
            file_id: "AgAC".into(),
            url: "https://p/1.jpg".into(),
        };
        match cached_media_payload(&with_id, true) {
            MediaItemPayload::Photo {
                media,
                has_spoiler,
                file_id,
                ..
            } => {
                assert_eq!(media, "AgAC");
                assert!(file_id, "a cached send must go by file id");
                assert!(has_spoiler);
            }
            _ => panic!("expected a photo payload"),
        }

        let degraded = CachedMedia {
            kind: CachedMediaKind::Video,
            file_id: String::new(),
            url: "https://v/1.mp4".into(),
        };
        match cached_media_payload(&degraded, false) {
            MediaItemPayload::Video {
                media,
                file_id,
                fallback_url,
                ..
            } => {
                assert_eq!(media, "https://v/1.mp4");
                assert!(!file_id, "a degraded send must go by URL");
                assert!(fallback_url.is_none());
            }
            _ => panic!("expected a video payload"),
        }
    }

    /// The caption-quote threshold matches the post's text inside the caption,
    /// so a long-text cache hit is quoted and a short-text one is not.
    #[tokio::test]
    async fn cache_hit_quotes_a_long_text_caption() {
        let mut stores = TestStores::new();
        stores.config_mut().caption_quote_text_chars = 3;
        let prefix = "https://x.com/u/status/1\n<a href=\"au\">a</a>: ";

        for (text, expected) in [
            (
                "abc",
                format!("{prefix}<blockquote expandable>abc</blockquote>"),
            ),
            ("ab", format!("{prefix}ab")),
        ] {
            let sender = MockSender::scripted(vec![Outcome::GroupOk], permanent_error);
            let ctx = stores.ctx(&sender);
            let mut entry = cached_photo();
            entry.caption = format!("{prefix}{text}");
            entry.title = String::new();
            entry.content = text.into();
            stores.link_cache().put("twitter:1", &entry).await;

            url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

            assert_eq!(sender.captions(), vec![expected], "text {text:?}");
        }
    }

    #[tokio::test]
    async fn unsupported_url_is_ignored_silently() {
        let stores = TestStores::new();
        let sender = MockSender::scripted(vec![], permanent_error);
        let ctx = stores.ctx(&sender);

        // No cache key → the fetch dispatcher returns Ok(None) without any
        // network; nothing is sent or replied.
        url_media(
            &ctx,
            1,
            2,
            "https://example.com/not-a-post",
            PostSend::FromChat,
        )
        .await;
        assert_eq!(sender.calls(), vec!["send_chat_action"]);
    }

    // ── One fetch per post ───────────────────────────────────────────────

    /// Two callers asking for the same post while its fetch is in flight run
    /// one fetch between them: the duplicate (a second chat, a batch forward
    /// and a retry) waits for that result instead of paying for its own.
    #[tokio::test]
    async fn concurrent_callers_share_one_fetch() {
        let map = parking_lot::Mutex::new(HashMap::new());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            7u32
        };

        let (first, second) = tokio::join!(
            shared_fetch(&map, "twitter:1", fetch),
            shared_fetch(&map, "twitter:1", fetch)
        );

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(*first, 7);
        assert!(std::sync::Arc::ptr_eq(&first, &second), "one shared result");
        assert!(
            map.lock().is_empty(),
            "the entry must not outlive the fetch"
        );
    }

    /// The dedup is *concurrent* only. A caller arriving after the fetch
    /// settled fetches again: the source may have changed, and a failure is
    /// deliberately not cached (the user is told to try again).
    #[tokio::test]
    async fn a_later_call_fetches_again() {
        let map = parking_lot::Mutex::new(HashMap::new());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let counting = |value: u32| {
            let calls = &calls;
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                value
            }
        };

        let first = shared_fetch(&map, "twitter:1", || counting(1)).await;
        let second = shared_fetch(&map, "twitter:1", || counting(2)).await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!((*first, *second), (1, 2));
        assert!(!std::sync::Arc::ptr_eq(&first, &second));
    }

    /// A cancelled fetch must not strand the callers that joined it: a live
    /// entry whose sharer is gone holds a sender, and the waiters would wait
    /// for a value that can never come. They fetch for themselves.
    #[tokio::test]
    async fn a_cancelled_fetch_does_not_strand_waiters() {
        let map = parking_lot::Mutex::new(HashMap::new());
        let calls = std::sync::atomic::AtomicUsize::new(0);

        // A fetch that never finishes, cancelled by the timeout below once it
        // has installed its entry.
        let slow = shared_fetch(&map, "twitter:1", || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<()>().await;
            0u32
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), slow)
                .await
                .is_err(),
            "the sharer must still be waiting when it is cancelled"
        );

        // Its entry is gone, and the next caller fetches its own value.
        let value = shared_fetch(&map, "twitter:1", || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            5u32
        })
        .await;
        assert_eq!(*value, 5);
        assert!(map.lock().is_empty());
    }

    // ── Send modes: the URL flow vs `/test` ─────────────────────────────

    /// A chat that has both post-send actions configured.
    async fn seed_post_send_settings(ctx: &AppContext<'_>) {
        ctx.chat_store
            .update(1, |data| {
                data.forward_channel_id = Some(2);
                data.edit_before_forward = true;
            })
            .await;
    }

    #[tokio::test]
    async fn chat_settings_apply_to_the_normal_link_flow() {
        let stores = TestStores::new();
        let sender =
            MockSender::scripted(vec![Outcome::GroupOk, Outcome::MessageOk], permanent_error);
        let ctx = stores.ctx(&sender);
        stores.link_cache().put("twitter:1", &cached_photo()).await;
        seed_post_send_settings(&ctx).await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

        // Media group, then the edit prompt (edit-before-forward wins over the
        // channel forward, which only runs once the prompt is confirmed).
        assert_eq!(
            sender.calls(),
            vec!["send_chat_action", "send_media_group", "send_message"]
        );
    }

    #[tokio::test]
    async fn test_mode_sends_the_media_without_forwarding_or_editing() {
        let stores = TestStores::new();
        // Only the group send is scripted: any forward (copy_messages) or edit
        // prompt (send_message) would panic with "unexpected outcome".
        let sender = MockSender::scripted(vec![Outcome::GroupOk], permanent_error);
        let ctx = stores.ctx(&sender);
        stores.link_cache().put("twitter:1", &cached_photo()).await;
        seed_post_send_settings(&ctx).await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::Suppressed).await;

        assert_eq!(sender.calls(), vec!["send_chat_action", "send_media_group"]);
        // The send is otherwise ordinary: the post stays cached (this is also
        // the retention control for the eviction case above).
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_some()
        );
    }

    #[test]
    fn send_mode_decides_whether_chat_actions_ride_along() {
        let chat = ChatData {
            forward_channel_id: Some(2),
            edit_before_forward: true,
            ..ChatData::default()
        };

        let with_chat = build_send_task(
            &chat,
            1,
            2,
            "https://x.com/u/status/1".into(),
            "cap".into(),
            vec![],
            None,
            PostSend::FromChat,
        );
        let Task::SendMediaSequence {
            edit_before_forward,
            forward_channel_id,
            ..
        } = with_chat
        else {
            panic!("expected a media sequence task");
        };
        assert!(edit_before_forward);
        assert_eq!(forward_channel_id, Some(2));

        let suppressed = build_send_task(
            &chat,
            1,
            2,
            "https://x.com/u/status/1".into(),
            "cap".into(),
            vec![],
            None,
            PostSend::Suppressed,
        );
        let Task::SendMediaSequence {
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            ..
        } = suppressed
        else {
            panic!("expected a media sequence task");
        };
        assert!(!edit_before_forward, "`/test` must not open an edit prompt");
        assert_eq!(forward_channel_id, None, "`/test` must not forward");
        // Dead-letter notification still reaches the chat that asked.
        assert_eq!(notify_chat_id, Some(1));
    }

    #[test]
    fn only_url_carrying_entities_yield_a_link() {
        use teloxide::types::MessageEntityKind;
        let post = "https://x.com/u/status/1";
        assert_eq!(
            url_of(&MessageEntityKind::Url, post),
            Some(post.to_string())
        );
        // A text link keeps its target, not the words the user sees.
        assert_eq!(
            url_of(
                &MessageEntityKind::TextLink {
                    url: url::Url::parse(post).unwrap()
                },
                "the post"
            ),
            Some(post.to_string())
        );
        for kind in [
            MessageEntityKind::Bold,
            MessageEntityKind::Code,
            MessageEntityKind::Hashtag,
            MessageEntityKind::Mention,
        ] {
            assert_eq!(url_of(&kind, "#nope"), None, "{kind:?}");
        }
        // Plain text that is no URL still comes back when it is a `Url` entity
        // (the fetch layer ignores what no site claims, and the user gets the
        // one explanatory reply it produces).
        assert_eq!(
            url_of(&MessageEntityKind::Url, "https://example.com/x"),
            Some("https://example.com/x".to_string())
        );
    }

    #[test]
    fn dedupe_by_post_and_by_exact_text() {
        // Two variants of one post, and a text link to the same post: one entry,
        // the first one seen.
        assert_eq!(
            dedupe_urls(vec![
                "https://x.com/u/status/1".into(),
                "https://x.com/u/status/1/photo/1".into(),
                "https://x.com/u/status/1".into(),
            ]),
            vec!["https://x.com/u/status/1"]
        );
        // Different posts both survive, in order.
        assert_eq!(
            dedupe_urls(vec![
                "https://x.com/a/status/1".into(),
                "https://x.com/b/status/2".into(),
            ]),
            vec!["https://x.com/a/status/1", "https://x.com/b/status/2"]
        );
        // A URL no site claims: exact-string dedup only.
        assert_eq!(
            dedupe_urls(vec![
                "https://example.com/a".into(),
                "https://example.com/a".into(),
                "https://example.com/b".into(),
            ]),
            vec!["https://example.com/a", "https://example.com/b"]
        );
        assert!(dedupe_urls(vec![]).is_empty());
    }

    fn queued_task(media: &str, batch_index: usize, sent: Vec<i64>) -> Task {
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
            media_batches: vec![vec![photo_item(media, false, false)]],
            batch_index,
            sent_message_ids: sent,
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: true,
            forward_channel_id: Some(2),
            notify_chat_id: Some(1),
            notify_message_id: Some(2),
            cache_data: None,
        }
    }

    #[test]
    fn only_tasks_missing_a_local_file_need_a_refetch() {
        // A URL send needs nothing.
        assert!(!needs_refetch(&queued_task("https://cdn/1.jpg", 0, vec![])));
        // A local path that is still there (a survived temp file) needs nothing.
        let dir = tempfile::tempdir().unwrap();
        let alive = dir.path().join("ugoira.mp4");
        std::fs::write(&alive, b"x").unwrap();
        assert!(!needs_refetch(&queued_task(
            alive.to_str().unwrap(),
            0,
            vec![]
        )));
        // A local path the restart took away does.
        assert!(needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            0,
            vec![]
        )));
        // A partially delivered album is left to its own retry path.
        assert!(!needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            1,
            vec![7]
        )));
        assert!(!needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            0,
            vec![7]
        )));
        // A channel copy holds no media.
        assert!(!needs_refetch(&Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            notify_chat_id: None,
            notify_message_id: None,
        }));
    }

    #[test]
    fn apply_refresh_keeps_the_delivery_envelope() {
        let task = queued_task("/nonexistent-ugoira.mp4", 0, vec![]);
        let fresh = Refetched {
            caption: "fresh caption".into(),
            items: vec![photo_item("https://cdn/fresh.jpg", true, false)],
            cache_data: None,
        };
        match apply_refresh(&task, &fresh).expect("a repairable task") {
            Task::SendMediaSequence {
                chat_id,
                reply_to_message_id,
                caption,
                media_batches,
                batch_index,
                sent_message_ids,
                source_url,
                edit_before_forward,
                forward_channel_id,
                notify_chat_id,
                notify_message_id,
                ..
            } => {
                // Same delivery: chat, reply, forward/edit settings, notify.
                assert_eq!((chat_id, reply_to_message_id), (1, 2));
                assert!(edit_before_forward);
                assert_eq!(forward_channel_id, Some(2));
                assert_eq!((notify_chat_id, notify_message_id), (Some(1), Some(2)));
                assert_eq!(source_url, "https://x.com/u/status/1");
                // Fresh media, and nothing of it counted as sent yet.
                assert_eq!(caption, "fresh caption");
                assert!(
                    matches!(
                        &media_batches[0][0],
                        MediaItemPayload::Photo { media, .. } if media == "https://cdn/fresh.jpg"
                    ),
                    "fresh media must replace the lost local file"
                );
                assert!(matches!(
                    media_batches[0][0],
                    MediaItemPayload::Photo {
                        has_spoiler: true,
                        ..
                    }
                ));
                assert_eq!((batch_index, sent_message_ids.len()), (0, 0));
            }
            other => panic!("expected a media sequence, got {other:?}"),
        }
    }

    /// The whole repair against a real post: a queued row whose media is a local
    /// file the restart took away is re-fetched from its `source_url` and
    /// rewritten in place, so the retry can still deliver it.
    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to public.api.bsky.app"]
    async fn live_repair_refetches_a_lost_local_media_row() {
        let stores = TestStores::new();
        // An empty script: the repair must not need to tell the user anything.
        let sender = MockSender::scripted(vec![], permanent_error);
        let ctx = stores.ctx(&sender);
        let mut task = queued_task("/nonexistent-ugoira.mp4", 0, vec![]);
        if let Task::SendMediaSequence { source_url, .. } = &mut task {
            *source_url = "https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224".into();
        }
        stores
            .task_queue()
            .enqueue(serde_json::to_value(&task).unwrap(), crate::db::now_f64())
            .await
            .unwrap();

        assert_eq!(repair_lost_local_media(&ctx).await, 1);

        let updated: Task = serde_json::from_value(stores.queued_payload().await).unwrap();
        match updated {
            Task::SendMediaSequence {
                media_batches,
                batch_index,
                sent_message_ids,
                caption,
                ..
            } => {
                let media: Vec<String> = media_batches
                    .iter()
                    .flatten()
                    .map(|item| match item {
                        MediaItemPayload::Photo { media, .. }
                        | MediaItemPayload::Video { media, .. }
                        | MediaItemPayload::Animation { media, .. } => media.clone(),
                    })
                    .collect();
                assert!(!media.is_empty(), "the fresh fetch yielded no media");
                assert!(
                    media.iter().all(|m| m.starts_with("http")),
                    "the retry must be uploadable from URLs again: {media:?}"
                );
                assert_eq!((batch_index, sent_message_ids.len()), (0, 0));
                assert!(!caption.is_empty());
            }
            other => panic!("expected a repaired media sequence, got {other:?}"),
        }
        // The post was re-read, not re-delivered: nothing was sent.
        assert!(sender.calls().is_empty(), "{:?}", sender.calls());
    }

    #[test]
    fn fetch_errors_map_to_distinct_user_messages() {
        use x_media::site::FetchError;

        let disabled = fetch_error_message(&FetchError::Disabled { site: "pixiv" });
        assert_eq!(disabled, "Pixiv support is disabled on this bot.");
        assert_eq!(
            fetch_error_message(&FetchError::NotFound),
            "Post not found (deleted, private or unavailable)."
        );
        let sensitive = fetch_error_message(&FetchError::Sensitive);
        assert!(sensitive.contains("TWITTER_AUTH_TOKEN"), "{sensitive}");
        let blocked = fetch_error_message(&FetchError::Blocked);
        assert!(blocked.contains("refused"), "{blocked}");
    }

    #[test]
    fn action_hint_follows_the_media_kind() {
        use MediaItemPayload::{Animation, Photo, Video};

        let photo = || Photo {
            media: "https://p/1.jpg".into(),
            has_spoiler: false,
            fallback_url: None,
            file_id: false,
        };
        let video = || Video {
            media: "https://v/1.mp4".into(),
            has_spoiler: false,
            thumbnail: None,
            fallback_url: None,
            file_id: false,
        };

        // Unknown before the fetch: the pipeline starts on "typing".
        assert!(matches!(ActionHint::Typing.action(), ChatAction::Typing));
        assert!(matches!(
            ActionHint::for_items(&[photo()]).action(),
            ChatAction::UploadPhoto
        ));
        assert!(matches!(
            ActionHint::for_items(&[
                video(),
                Animation {
                    media: "https://v/2.mp4".into(),
                    has_spoiler: false,
                    file_id: false,
                }
            ])
            .action(),
            ChatAction::UploadVideo
        ));
        // A mixed post takes the photo label: `photos_first` always leads with
        // a photo, which is what Telegram shows.
        assert!(matches!(
            ActionHint::for_items(&[video(), photo()]).action(),
            ChatAction::UploadPhoto
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_pipeline_keeps_the_chat_action_alive() {
        let sender = MockSender::scripted(vec![], permanent_error);
        let hint = parking_lot::Mutex::new(ActionHint::Typing);
        // Three refresh windows of work: Telegram would have dropped the
        // indicator twice without the keep-alive.
        let pipeline = async { tokio::time::sleep(ACTION_REFRESH * 3).await };

        run_with_chat_action(&sender, 1, &hint, pipeline).await;

        let actions = sender
            .calls()
            .iter()
            .filter(|call| **call == "send_chat_action")
            .count();
        assert_eq!(actions, 3, "expected the initial action plus two refreshes");
    }
}
