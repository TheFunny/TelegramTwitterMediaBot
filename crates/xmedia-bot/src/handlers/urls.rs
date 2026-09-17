//! URL extraction and the per-URL media pipeline: bounded job channel +
//! worker pool, link-cache fast path, fetch, task build and send dispatch.

use super::{log_key, reply};
use crate::ctx::{AppContext, CONTEXT};
use crate::link_cache::{CachedMediaKind, CachedPost};
use crate::send::{self, MediaItemPayload, Task};
use crate::state::ChatData;
use std::collections::HashSet;
use std::sync::LazyLock;
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
                    Some((message, url)) => {
                        url_media(
                            &CONTEXT,
                            message.chat.id.0,
                            message.id.0 as i64,
                            &url,
                            PostSend::FromChat,
                        )
                        .await
                    }
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
) {
    let result = match task {
        Task::SendAnimation { .. } => send::send_animation(ctx, task).await,
        Task::SendMediaSequence { .. } => send::send_media_sequence(ctx, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    };
    match result {
        Ok(message_ids) => {
            log::info!(
                "sent {} message(s) for [key={}]",
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
                "send for [key={}] failed, queued for retry in {delay_seconds:.1}s",
                log_key(url)
            );
            send::enqueue_retry(ctx.task_queue, *task, delay_seconds).await;
            let _ = reply(
                ctx.sender,
                chat_id,
                reply_to,
                "Send failed. Task queued for retry.",
            )
            .await;
        }
        Err(send::SendError::Permanent {
            message: err_message,
            task,
        }) => {
            send::settle_task(ctx, &task, send::Settled::Failed).await;
            log::error!("send for {url} failed permanently: {err_message}");
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
    if items.len() == 1 && matches!(items[0], MediaItemPayload::Animation { .. }) {
        Task::SendAnimation {
            chat_id,
            reply_to_message_id,
            caption,
            animation: items.into_iter().next().unwrap(),
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(reply_to_message_id),
            cache_data,
        }
    } else {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id,
            caption,
            // Photos first so a mixed photo+video group starts with a photo
            // (Telegram's sendMediaGroup rule); order within each kind is kept.
            media_batches: send::chunk_media_items(send::photos_first(items)),
            batch_index: 0,
            sent_message_ids: vec![],
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(reply_to_message_id),
            cache_data,
        }
    }
}

/// The per-URL pipeline: link cache → fetch → build → send → post-send.
///
/// `post_send` selects whether the chat's forward/edit settings apply: the URL
/// workers pass [`PostSend::FromChat`], the `/test` command
/// [`PostSend::Suppressed`]. Everything else (cache write, retry enqueue,
/// dead-letter notification) is identical.
pub(crate) async fn url_media(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to_message_id: i64,
    url: &str,
    post_send: PostSend,
) {
    let reply_to = MessageId(reply_to_message_id as i32);
    if let Err(e) = ctx
        .sender
        .send_chat_action(ChatId(chat_id), ChatAction::Typing)
        .await
    {
        log::error!("send_chat_action failed: {e}");
    }

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
                &cached.content,
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
            chat_id,
            reply_to_message_id,
            cached.url.clone(),
            caption,
            items,
            Some(cached),
            post_send,
        );
        dispatch_send(ctx, chat_id, reply_to, &task, url).await;
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
                ctx.sender,
                chat_id,
                reply_to,
                "Failed to fetch media from this link.",
            )
            .await;
        }
        Ok(Some(mut fetched)) => {
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
                .get(fetched.site_name())
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
            // local file would be gone by the time the retry sends it.
            if let Some(dir) = fetched.take_keep_alive() {
                send::KEEP_ALIVE.lock().push(dir);
            }
            dispatch_send(ctx, chat_id, reply_to, &task, url).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::TestStores;
    use crate::link_cache::CachedMedia;
    use crate::media_sender::test_support::{MockSender, Outcome};
    use std::time::Duration;
    use teloxide::{ApiError, RequestError};

    fn permanent_error() -> RequestError {
        RequestError::Api(ApiError::Unknown(
            "Bad Request: message is not modified".into(),
        ))
    }

    fn cached_photo_entry() -> CachedPost {
        CachedPost {
            url: "https://x.com/u/status/1".into(),
            caption: "cap".into(),
            title: "t".into(),
            content: "c".into(),
            author: "a".into(),
            author_url: "au".into(),
            tags: "".into(),
            sensitive: false,
            media: vec![CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: "file-1".into(),
            }],
        }
    }

    #[tokio::test]
    async fn cache_hit_sends_file_ids_and_invalidates_on_permanent_failure() {
        let stores = TestStores::new();
        let sender = MockSender::scripted(
            vec![Outcome::GroupErr, Outcome::MessageErr],
            permanent_error,
        );
        let ctx = stores.ctx(&sender);
        stores
            .link_cache()
            .put("twitter:1", &cached_photo_entry())
            .await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

        // The cached file id went out as a group send; the permanent failure
        // then triggered the fire-and-forget reply (its mock error is fine).
        assert_eq!(
            sender.calls(),
            vec!["send_chat_action", "send_media_group", "send_message"]
        );
        // The stale cache entry was invalidated so the next request re-fetches.
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cache_hit_success_keeps_the_cache_entry() {
        let stores = TestStores::new();
        let sender = MockSender::scripted(vec![Outcome::GroupOk], permanent_error);
        let ctx = stores.ctx(&sender);
        stores
            .link_cache()
            .put("twitter:1", &cached_photo_entry())
            .await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::FromChat).await;

        assert_eq!(sender.calls(), vec!["send_chat_action", "send_media_group"]);
        // Success must not evict the entry.
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_some()
        );
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
            let mut entry = cached_photo_entry();
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
        stores
            .link_cache()
            .put("twitter:1", &cached_photo_entry())
            .await;
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
        stores
            .link_cache()
            .put("twitter:1", &cached_photo_entry())
            .await;
        seed_post_send_settings(&ctx).await;

        url_media(&ctx, 1, 2, "https://x.com/u/status/1", PostSend::Suppressed).await;

        assert_eq!(sender.calls(), vec!["send_chat_action", "send_media_group"]);
        // The send is otherwise ordinary: the post stays cached.
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
}
