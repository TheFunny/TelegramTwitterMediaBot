//! Task payloads and the send/forward executors, split by concern:
//! [`input_media`] builds Telegram input types from payloads, [`upload`] is
//! the download-and-reupload fallback (Telegram's own fetch of a media URL is
//! blocked by hotlink protection, so the bot downloads the file itself and
//! uploads it via multipart), [`post_send`] covers everything around a send
//! (cache write, keep-alive media, settlement, post-send actions, queue
//! entry points). This module keeps the payload types, the error
//! classification and the senders themselves.

mod input_media;
mod post_send;
mod upload;

use crate::ctx::AppContext;
use crate::handlers::log_key;
use crate::link_cache::{CachedMedia, CachedMediaKind, CachedPost};
use crate::media_sender::MediaSender;
use input_media::{build_media_group, input_file_for, item_url};
use post_send::{cache_animation_send, cache_sent_task};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, InputMedia, MessageId};
use teloxide::{ApiError, RequestError};
use upload::{FallbackError, PreparedItem, prepare_upload_item, send_batch_via_upload};

// The crate-facing API of this module lives in its submodules; re-export the
// parts other modules use so call sites stay `send::x`.
pub(crate) use post_send::{
    EDIT_PROMPT_EXPIRED_TEXT, KEEP_ALIVE, Settled, dead_letter_notify, enqueue_retry, handle_task,
    post_send_actions, settle_task,
};

/// One process-wide Bot for queue workers. Building a fresh Bot (and its HTTP
/// client) per queue task was pure waste; forced at startup in main so a
/// missing token fails fast instead of on the first task.
pub static BOT: LazyLock<Bot> = LazyLock::new(Bot::from_env);

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaItemPayload {
    Photo {
        media: String,
        has_spoiler: bool,
        /// Smaller variant used when the primary media exceeds Telegram's
        /// size limits.
        #[serde(default)]
        fallback_url: Option<String>,
        /// `media` is a Telegram file id (link-cache hit), not a URL.
        #[serde(default)]
        file_id: bool,
    },
    Video {
        media: String,
        has_spoiler: bool,
        thumbnail: Option<String>,
        #[serde(default)]
        fallback_url: Option<String>,
        /// `media` is a Telegram file id (link-cache hit), not a URL.
        #[serde(default)]
        file_id: bool,
    },
    Animation {
        media: String,
        has_spoiler: bool,
        /// `media` is a Telegram file id (link-cache hit), not a URL.
        #[serde(default)]
        file_id: bool,
    },
}

impl MediaItemPayload {
    fn fallback_url(&self) -> Option<&str> {
        match self {
            MediaItemPayload::Photo { fallback_url, .. }
            | MediaItemPayload::Video { fallback_url, .. } => fallback_url.as_deref(),
            MediaItemPayload::Animation { .. } => None,
        }
    }

    /// The cover-frame URL for videos (used by the upload fallback, which
    /// otherwise drops the thumbnail the URL-send path applies).
    fn thumbnail_url(&self) -> Option<&str> {
        match self {
            MediaItemPayload::Video { thumbnail, .. } => thumbnail.as_deref(),
            MediaItemPayload::Photo { .. } | MediaItemPayload::Animation { .. } => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Task {
    SendMediaSequence {
        chat_id: i64,
        reply_to_message_id: i64,
        caption: String,
        media_batches: Vec<Vec<MediaItemPayload>>,
        batch_index: usize,
        sent_message_ids: Vec<i64>,
        source_url: String,
        edit_before_forward: bool,
        forward_channel_id: Option<i64>,
        notify_chat_id: Option<i64>,
        notify_message_id: Option<i64>,
        /// Raw render data captured on a cache miss; the send fills in the
        /// Telegram file ids and persists the entry (see `link_cache`).
        #[serde(default)]
        cache_data: Option<CachedPost>,
    },
    SendAnimation {
        chat_id: i64,
        reply_to_message_id: i64,
        caption: String,
        animation: MediaItemPayload,
        source_url: String,
        edit_before_forward: bool,
        forward_channel_id: Option<i64>,
        notify_chat_id: Option<i64>,
        notify_message_id: Option<i64>,
        /// Raw render data captured on a cache miss; the send fills in the
        /// Telegram file id and persists the entry (see `link_cache`).
        #[serde(default)]
        cache_data: Option<CachedPost>,
    },
    ForwardMessages {
        from_chat_id: i64,
        to_chat_id: i64,
        message_ids: Vec<i64>,
        notify_chat_id: Option<i64>,
        notify_message_id: Option<i64>,
    },
}

impl Task {
    fn cache_data(&self) -> Option<&CachedPost> {
        match self {
            Task::SendMediaSequence { cache_data, .. } | Task::SendAnimation { cache_data, .. } => {
                cache_data.as_ref()
            }
            Task::ForwardMessages { .. } => None,
        }
    }

    fn source_url(&self) -> Option<&str> {
        match self {
            Task::SendMediaSequence { source_url, .. } | Task::SendAnimation { source_url, .. } => {
                Some(source_url)
            }
            Task::ForwardMessages { .. } => None,
        }
    }

    /// True when the media payloads are Telegram file ids from the link cache
    /// (a cached file id that goes permanently bad should be dropped so the
    /// next request re-fetches).
    fn is_cached_send(&self) -> bool {
        self.cache_data().is_some_and(|c| !c.media.is_empty())
    }

    /// All media payloads of this task (sequence batches flattened plus the
    /// lone animation).
    fn media_items(&self) -> Vec<&MediaItemPayload> {
        match self {
            Task::SendMediaSequence { media_batches, .. } => {
                media_batches.iter().flatten().collect()
            }
            Task::SendAnimation { animation, .. } => {
                std::slice::from_ref(animation).iter().collect()
            }
            Task::ForwardMessages { .. } => Vec::new(),
        }
    }

    /// Local file paths referenced by this task's media (ugoira / bsky remux
    /// MP4 and the like); empty for URL or Telegram file-id sends.
    fn local_media_paths(&self) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for item in self.media_items() {
            let is_file_id = match item {
                MediaItemPayload::Photo { file_id, .. }
                | MediaItemPayload::Video { file_id, .. }
                | MediaItemPayload::Animation { file_id, .. } => *file_id,
            };
            if is_file_id {
                continue;
            }
            let media = item_url(item);
            if !media.starts_with("http://") && !media.starts_with("https://") {
                out.push(std::path::PathBuf::from(media));
            }
        }
        out
    }
}

/// Telegram file id of the message's media, matched to the payload kind.
fn file_id_of_message(message: &Message, item: &MediaItemPayload) -> Option<String> {
    match item {
        // `photo()` returns all sizes, smallest first — the largest carries
        // the file id of the sent media.
        MediaItemPayload::Photo { .. } => message
            .photo()
            .and_then(|sizes| sizes.last())
            .map(|p| p.file.id.to_string()),
        MediaItemPayload::Video { .. } => message.video().map(|v| v.file.id.to_string()),
        MediaItemPayload::Animation { .. } => message.animation().map(|a| a.file.id.to_string()),
    }
}

fn kind_of_item(item: &MediaItemPayload) -> CachedMediaKind {
    match item {
        MediaItemPayload::Photo { .. } => CachedMediaKind::Photo,
        MediaItemPayload::Video { .. } => CachedMediaKind::Video,
        MediaItemPayload::Animation { .. } => CachedMediaKind::Animation,
    }
}

/// Collects the Telegram file ids of a sent media group, aligned to the
/// batch's items.
fn collect_file_ids(messages: &[Message], batch: &[MediaItemPayload], out: &mut Vec<CachedMedia>) {
    for (message, item) in messages.iter().zip(batch.iter()) {
        if let Some(file_id) = file_id_of_message(message, item) {
            out.push(CachedMedia {
                kind: kind_of_item(item),
                file_id,
            });
        }
    }
}

/// Telegram's `sendMediaGroup` accepts 2–10 items per group; 10 (not the older
/// 9) means a 10-image post arrives as one album instead of two messages.
pub const MAX_MEDIA_GROUP: usize = 10;

/// Splits media into batches of at most [`MAX_MEDIA_GROUP`] items, moving the
/// items out (no per-item clone).
pub fn chunk_media_items<T>(items: Vec<T>) -> Vec<Vec<T>> {
    let mut items = items.into_iter();
    let mut batches = Vec::new();
    loop {
        let batch: Vec<T> = items.by_ref().take(MAX_MEDIA_GROUP).collect();
        if batch.is_empty() {
            return batches;
        }
        batches.push(batch);
    }
}

/// Orders media for a Telegram media group: when photos and videos are
/// mixed, the first item must be a photo (Telegram's sendMediaGroup rule).
/// Stable sort keeps the source order within each kind; a lone animation is
/// untouched (it takes the SendAnimation path before this runs).
pub fn photos_first(items: Vec<MediaItemPayload>) -> Vec<MediaItemPayload> {
    let mut items = items;
    items.sort_by_key(|item| match item {
        MediaItemPayload::Photo { .. } => 0,
        MediaItemPayload::Video { .. } | MediaItemPayload::Animation { .. } => 1,
    });
    items
}

/// Exponential backoff with jitter, capped at 30s.
pub fn retry_delay_seconds(attempts: u32) -> f64 {
    let jitter: f64 = rand::random_range(0.2..0.8);
    (2f64.powi(attempts as i32) + jitter).min(30.0)
}

/// Telegram's servers failed to fetch a media URL (hotlink protection etc.):
/// these errors are handled by the download-and-reupload fallback, NOT by a
/// queue retry (resending the URL cannot succeed).
pub fn is_media_fetch_failure(e: &ApiError) -> bool {
    const MARKERS: [&str; 6] = [
        "webpage_media_empty",
        "media_empty",
        "empty_web_media",
        "webpage_curl_failed",
        "timeout",
        // Oversized photos (width + height > 10000 px) are rejected on URL
        // sends too; route them to the download-and-resize fallback.
        "photo_invalid_dimensions",
    ];
    let description = e.to_string().to_lowercase();
    MARKERS.iter().any(|marker| description.contains(marker))
}

/// Telegram reported the media file as too large (HTTP 413 on multipart
/// upload, or a "too large" message for URL-fetched media). These errors are
/// handled by the size-check fallback (use a smaller media URL), NOT by a
/// queue retry.
pub fn is_size_error(e: &ApiError) -> bool {
    if matches!(e, ApiError::RequestEntityTooLarge) {
        return true;
    }
    let description = e.to_string().to_lowercase();
    ["too large", "too big"]
        .iter()
        .any(|marker| description.contains(marker))
}

/// Task-free classification of a Telegram request error. The callers attach
/// the (updated) task when building a [`SendError`].
pub enum Classification {
    Retryable {
        delay_seconds: f64,
    },
    Permanent {
        message: String,
    },
    /// Handled by the download fallback, not a queue retry.
    MediaFetchFailure,
}

pub fn classify_request_error(e: &RequestError) -> Classification {
    match e {
        RequestError::RetryAfter(seconds) => Classification::Retryable {
            delay_seconds: seconds.seconds() as f64,
        },
        RequestError::Network(_) => Classification::Retryable {
            delay_seconds: retry_delay_seconds(0),
        },
        RequestError::Api(api) if is_media_fetch_failure(api) => Classification::MediaFetchFailure,
        RequestError::Api(api) => Classification::Permanent {
            message: api.to_string(),
        },
        RequestError::MigrateToChatId(_)
        | RequestError::InvalidJson { .. }
        | RequestError::Io(_) => Classification::Permanent {
            message: e.to_string(),
        },
    }
}

/// Task boxed to keep the error size within `result_large_err` limits.
#[derive(Debug)]
pub enum SendError {
    Retryable { delay_seconds: f64, task: Box<Task> },
    Permanent { message: String, task: Box<Task> },
}
fn classify_to_send_error(e: &RequestError, task: Task, fetch_failure_label: &str) -> SendError {
    match classify_request_error(e) {
        Classification::Retryable { delay_seconds } => SendError::Retryable {
            delay_seconds,
            task: Box::new(task),
        },
        Classification::Permanent { message } => SendError::Permanent {
            message,
            task: Box::new(task),
        },
        Classification::MediaFetchFailure => SendError::Permanent {
            message: fetch_failure_label.into(),
            task: Box::new(task),
        },
    }
}

impl SendError {
    /// Attaches the (updated) task to a task-free [`FallbackError`] from the
    /// download/upload pipeline. [`FallbackError::MediaTooLarge`] never
    /// escapes the pipeline (it is handled by falling back to the smaller
    /// URL), so it is unreachable here.
    fn from_fallback(f: FallbackError, task: Task) -> SendError {
        match f {
            FallbackError::Retryable { delay_seconds } => SendError::Retryable {
                delay_seconds,
                task: Box::new(task),
            },
            FallbackError::Permanent { message } => SendError::Permanent {
                message,
                task: Box::new(task),
            },
            FallbackError::MediaTooLarge => unreachable!("handled inside the upload fallback"),
        }
    }
}

fn updated_sequence_task(task: &Task, batch_index: usize, sent_message_ids: Vec<i64>) -> Task {
    let mut updated = task.clone();
    match &mut updated {
        Task::SendMediaSequence {
            batch_index: index,
            sent_message_ids: ids,
            ..
        } => {
            *index = batch_index;
            *ids = sent_message_ids;
        }
        _ => unreachable!("updated_sequence_task requires a SendMediaSequence task"),
    }
    updated
}

/// The caption's text tail: everything after the author link, provided it
/// really is the post's text.
///
/// `text` is the *escaped* title + content the caption embeds; the caption may
/// have been truncated inside it, in which case only its prefix is present, so
/// the tail only has to match the text's start. `None` for a caption with
/// another layout — pixiv's title-inside-a-link, a `/set_format` that moves
/// `{title}`/`{content}` off the author line — which is left unquoted instead
/// of guessing where the text begins.
fn text_tail<'c>(caption: &'c str, text: &str) -> Option<&'c str> {
    let (_, tail) = caption.rsplit_once("</a>: ")?;
    let visible = tail.strip_suffix('\u{2026}').unwrap_or(tail);
    (!visible.is_empty() && text.starts_with(visible)).then_some(tail)
}

/// The text a task's caption embeds, read from the same cache snapshot the
/// caption came from: `title` and `content` joined the way the sites' built-in
/// captions join them.
fn task_text(task: &Task) -> String {
    task.cache_data()
        .map(|data| x_media::site::compose_text(&data.title, &data.content))
        .unwrap_or_default()
}

/// Wraps the post's text inside the caption in an expandable blockquote once
/// that text is long enough that the message would otherwise be a wall of text
/// (`threshold` is `CAPTION_QUOTE_TEXT_CHARS`; `0` disables the wrap). The URL
/// and the author line stay outside the quote.
///
/// Applied at the send boundary, after the caller's `truncate_caption`:
/// Telegram measures a caption *after entities parsing*, so the tags cost no
/// length and a wrapped caption cannot exceed the 1024-character limit.
/// Retries replay the task's (unwrapped) caption, so the decision is remade on
/// every attempt — changing the threshold takes effect immediately.
///
/// A caption that already carries a blockquote is left as it is: the API
/// rejects nested ones ("all other entities can't contain each other"), and a
/// user-written `/set_format` template may contain one.
pub(crate) fn quote_long_caption<'a>(
    caption: &'a str,
    text: &str,
    threshold: usize,
) -> Cow<'a, str> {
    if threshold == 0 || caption.contains("<blockquote") || text.chars().count() < threshold {
        return Cow::Borrowed(caption);
    }
    let Some(tail) = text_tail(caption, text) else {
        return Cow::Borrowed(caption);
    };
    let prefix = &caption[..caption.len() - tail.len()];
    Cow::Owned(format!(
        "{prefix}<blockquote expandable>{tail}</blockquote>"
    ))
}

/// Sends the media batches starting at `task.batch_index`, extending
/// `sent_message_ids`. Returns all sent message ids on full success; on
/// failure returns a [`SendError`] whose task carries the resumed state.
pub async fn send_media_sequence(ctx: &AppContext<'_>, task: &Task) -> Result<Vec<i64>, SendError> {
    let Task::SendMediaSequence {
        chat_id,
        reply_to_message_id,
        caption,
        media_batches,
        batch_index,
        sent_message_ids,
        ..
    } = task
    else {
        unreachable!("send_media_sequence requires a SendMediaSequence task")
    };
    let chat_id = *chat_id;
    let reply_to = *reply_to_message_id;
    // A long post is quoted so the message reads as a card rather than a wall
    // of text; the text comes from the same cache snapshot as the caption.
    let text = task_text(task);
    let caption = quote_long_caption(caption, &text, ctx.config.caption_quote_text_chars);
    let mut sent = sent_message_ids.clone();
    // File ids accumulated across batches for the link cache. Only a fresh
    // (non-resumed) full send populates the cache.
    let mut cached_media: Vec<CachedMedia> = Vec::new();
    let fresh_send = *batch_index == 0 && sent.is_empty();
    for idx in *batch_index..media_batches.len() {
        let batch = &media_batches[idx];
        let caption = if idx == 0 {
            Some(caption.as_ref())
        } else {
            None
        };
        let items = match build_media_group(batch, caption) {
            Ok(items) => items,
            Err(message) => {
                return Err(SendError::Permanent {
                    message,
                    task: Box::new(updated_sequence_task(task, idx, sent)),
                });
            }
        };
        match ctx
            .sender
            .send_media_group(ChatId(chat_id), MessageId(reply_to as i32), items)
            .await
        {
            Ok(messages) => {
                log::debug!(
                    "media group batch {idx}/{} sent ({} item(s))",
                    media_batches.len(),
                    batch.len()
                );
                collect_file_ids(&messages, batch, &mut cached_media);
                sent.extend(messages.into_iter().map(|m| m.id.0 as i64));
            }
            Err(RequestError::Api(api)) if is_media_fetch_failure(&api) || is_size_error(&api) => {
                log::info!(
                    "Telegram could not fetch media for batch {idx} ({}), downloading and reuploading",
                    batch
                        .first()
                        .map(item_url)
                        .map(log_key)
                        .unwrap_or_else(|| "?".into())
                );
                match send_batch_via_upload(
                    ctx.sender,
                    chat_id,
                    reply_to,
                    batch,
                    caption,
                    updated_sequence_task(task, idx, sent.clone()),
                )
                .await
                {
                    Ok(messages) => {
                        collect_file_ids(&messages, batch, &mut cached_media);
                        sent.extend(messages.into_iter().map(|m| m.id.0 as i64));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => {
                return Err(classify_to_send_error(
                    &e,
                    updated_sequence_task(task, idx, sent),
                    "media fetch failed",
                ));
            }
        }
    }
    if fresh_send {
        cache_sent_task(ctx, task, cached_media).await;
    }
    Ok(sent)
}

async fn send_animation_inner(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: i64,
    caption: &str,
    spoiler: bool,
    file: InputFile,
) -> Result<Message, RequestError> {
    sender
        .send_animation(
            ChatId(chat_id),
            MessageId(reply_to as i32),
            caption,
            spoiler,
            file,
        )
        .await
}

/// Sends a lone animation (gif), URL first with the download fallback.
pub async fn send_animation(ctx: &AppContext<'_>, task: &Task) -> Result<Vec<i64>, SendError> {
    let Task::SendAnimation {
        chat_id,
        reply_to_message_id,
        caption,
        animation,
        ..
    } = task
    else {
        unreachable!("send_animation requires a SendAnimation task")
    };
    let chat_id = *chat_id;
    let reply_to = *reply_to_message_id;
    // Same long-post quoting as the media-group path (see
    // `quote_long_caption`); both sends below share this string.
    let text = task_text(task);
    let caption = quote_long_caption(caption, &text, ctx.config.caption_quote_text_chars);
    let (media_url, has_spoiler) = match animation {
        MediaItemPayload::Animation {
            media, has_spoiler, ..
        } => (media, *has_spoiler),
        MediaItemPayload::Photo { .. } | MediaItemPayload::Video { .. } => {
            unreachable!("SendAnimation carries an Animation payload")
        }
    };
    let url_file = match input_file_for(media_url) {
        Ok(file) => file,
        Err(message) => {
            return Err(SendError::Permanent {
                message,
                task: Box::new(task.clone()),
            });
        }
    };
    match send_animation_inner(
        ctx.sender,
        chat_id,
        reply_to,
        &caption,
        has_spoiler,
        url_file,
    )
    .await
    {
        Ok(message) => {
            let id = message.id.0 as i64;
            cache_animation_send(ctx, task, &message).await;
            Ok(vec![id])
        }
        Err(RequestError::Api(api)) if is_media_fetch_failure(&api) || is_size_error(&api) => {
            log::info!(
                "Telegram could not fetch animation URL, downloading and reuploading: [key={}]",
                log_key(media_url)
            );
            // Single-item local preparation — the same pipeline the media
            // group fallback uses (download with the upload-cap check,
            // downscale/transcode photos, smaller-URL fallback). Animations
            // have no smaller variant, so an oversized file surfaces as a
            // permanent error here.
            match prepare_upload_item(animation.clone(), 0, None).await {
                Ok(prepared) => {
                    let PreparedItem {
                        media, keep_alive, ..
                    } = prepared;
                    let InputMedia::Animation(animation) = media else {
                        unreachable!("an Animation payload prepares to InputMedia::Animation")
                    };
                    // Hold the temp file until the request completes.
                    let _keep_alive = keep_alive;
                    match send_animation_inner(
                        ctx.sender,
                        chat_id,
                        reply_to,
                        &caption,
                        has_spoiler,
                        animation.media,
                    )
                    .await
                    {
                        Ok(message) => {
                            let id = message.id.0 as i64;
                            cache_animation_send(ctx, task, &message).await;
                            Ok(vec![id])
                        }
                        Err(e) => Err(classify_to_send_error(
                            &e,
                            task.clone(),
                            "media fetch failed",
                        )),
                    }
                }
                Err(e) => Err(SendError::from_fallback(e, task.clone())),
            }
        }
        Err(e) => Err(classify_to_send_error(
            &e,
            task.clone(),
            "media fetch failed",
        )),
    }
}

/// Copies already-sent messages to the forward channel. No download fallback:
/// the files are already on Telegram's servers.
pub async fn forward_messages(ctx: &AppContext<'_>, task: &Task) -> Result<(), SendError> {
    let Task::ForwardMessages {
        from_chat_id,
        to_chat_id,
        message_ids,
        ..
    } = task
    else {
        unreachable!("forward_messages requires a ForwardMessages task")
    };
    let message_ids = message_ids
        .iter()
        .map(|id| MessageId(*id as i32))
        .collect::<Vec<_>>();
    match ctx
        .sender
        .copy_messages(
            ChatId(*to_chat_id),
            ChatId(*from_chat_id),
            message_ids.clone(),
        )
        .await
    {
        Ok(_) => {
            log::info!(
                "copied {} message(s) from {} to {}",
                message_ids.len(),
                from_chat_id,
                to_chat_id
            );
            Ok(())
        }
        Err(e) => Err(classify_to_send_error(
            &e,
            task.clone(),
            "media fetch failed",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::post_send::build_edit_markup;
    use super::upload::sniff_ext;
    use super::*;
    use crate::ctx::test_support::TestStores;
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn oversized_photo_boundary() {
        // The empirical Telegram limit: sum 10000 passes, 10001 fails.
        // Const-block asserts so clippy's assertions_on_constants stays quiet.
        const { assert!(crate::photo::PHOTO_MAX_DIMENSION_SUM == 10000) };
        const { assert!(6100 + 3900 <= crate::photo::PHOTO_MAX_DIMENSION_SUM) };
        const { assert!(6300 + 3730 > crate::photo::PHOTO_MAX_DIMENSION_SUM) };
    }

    #[test]
    fn chunk_media_items_sizes() {
        assert_eq!(chunk_media_items::<i32>(vec![]), Vec::<Vec<i32>>::new());
        assert_eq!(chunk_media_items((0..9).collect()).len(), 1);
        assert_eq!(chunk_media_items((0..10).collect()).len(), 1);
        assert_eq!(chunk_media_items((0..11).collect()).len(), 2);
        assert_eq!(chunk_media_items((0..25).collect()).len(), 3);
        assert_eq!(chunk_media_items((0..25).collect())[2].len(), 5);
        assert!(
            chunk_media_items((0..25).collect())
                .iter()
                .all(|c| c.len() <= MAX_MEDIA_GROUP)
        );
    }

    #[test]
    fn photos_first_orders_photos_before_videos() {
        use MediaItemPayload::{Animation, Photo, Video};
        let photo = |u: &str| Photo {
            media: u.into(),
            has_spoiler: false,
            fallback_url: None,
            file_id: false,
        };
        let video = |u: &str| Video {
            media: u.into(),
            has_spoiler: false,
            thumbnail: None,
            fallback_url: None,
            file_id: false,
        };
        let items = vec![
            video("https://v/1.mp4"),
            photo("https://p/1.jpg"),
            video("https://v/2.mp4"),
            photo("https://p/2.jpg"),
        ];
        let ordered = photos_first(items);
        // All photos first (stable: p1 before p2), then all videos in order.
        let kinds: Vec<&str> = ordered
            .iter()
            .map(|i| match i {
                Photo { media, .. } => media.as_str(),
                Video { media, .. } => media.as_str(),
                Animation { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "https://p/1.jpg",
                "https://p/2.jpg",
                "https://v/1.mp4",
                "https://v/2.mp4"
            ]
        );
        // Already-photos-first input is unchanged.
        let items = vec![photo("https://p/1.jpg"), video("https://v/1.mp4")];
        assert!(matches!(photos_first(items)[0], Photo { .. }));
    }

    #[test]
    fn retry_delay_seconds_bounds() {
        for attempts in 0..10 {
            let delay = retry_delay_seconds(attempts);
            assert!(delay >= 1.0, "attempts={attempts}: {delay}");
            assert!(delay <= 30.0, "attempts={attempts}: {delay}");
        }
    }

    #[test]
    fn edit_markup_lists_templates_sorted_then_confirm() {
        // Six names: a HashMap walk would land on this order by chance only
        // 1 time in 720.
        let templates: HashMap<String, String> = ["z", "a", "m", "q", "b", "y"]
            .into_iter()
            .map(|name| (name.to_string(), "[]".to_string()))
            .collect();
        let labels: Vec<String> = build_edit_markup(&templates)
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.text.clone())
            .collect();
        assert_eq!(
            labels,
            ["a", "b", "m", "q", "y", "z", "↩️ Confirm", "🛑 Skip"]
        );
    }

    #[test]
    fn edit_prompt_text_states_the_ttl_and_the_confirm_requirement() {
        use std::time::Duration;

        let text = super::post_send::edit_prompt_text(Duration::from_secs(24 * 3600));
        assert!(text.contains("Expires in 24h"), "{text}");
        assert!(text.contains("Confirm"), "{text}");
        // The wording of the whole point: no Confirm, no forward.
        assert!(text.contains("Nothing is forwarded"), "{text}");
        // Sub-hour TTLs must not render "0h".
        assert!(
            super::post_send::edit_prompt_text(Duration::from_secs(90)).contains("Expires in 1m")
        );
        assert!(
            super::post_send::edit_prompt_text(Duration::from_secs(30)).contains("Expires in 30s")
        );
    }

    #[test]
    fn is_media_fetch_failure_matches_markers() {
        for description in [
            "Bad Request: WEBPAGE_MEDIA_EMPTY",
            "Bad Request: media_empty",
            "Bad Request: EMPTY_WEB_MEDIA",
            "Bad Request: webpage_curl_failed",
            "Bad Request: request timeout",
            // Telegram sends the code in upper case; the comparison is against
            // the lower-cased description.
            "Bad Request: PHOTO_INVALID_DIMENSIONS: width and height must be <= 10000",
        ] {
            let api = ApiError::Unknown(description.to_string());
            assert!(is_media_fetch_failure(&api), "{description}");
        }
        for description in [
            "Bad Request: message is not modified",
            "Forbidden: bot was blocked by the user",
        ] {
            let api = ApiError::Unknown(description.to_string());
            assert!(!is_media_fetch_failure(&api), "{description}");
        }
    }

    #[test]
    fn is_size_error_matches_known_errors() {
        // 413 upload cap.
        let e = ApiError::RequestEntityTooLarge;
        assert!(is_size_error(&e), "{e:?}");
        // Unknown descriptions with size wording.
        for description in [
            "Bad Request: file is too large",
            "Bad Request: media is too big",
            "Bad Request: url file size is too big",
        ] {
            let api = ApiError::Unknown(description.to_string());
            assert!(is_size_error(&api), "{description}");
        }
        // Unrelated errors must not match.
        for description in [
            "Bad Request: WEBPAGE_MEDIA_EMPTY",
            "Bad Request: message is not modified",
        ] {
            let api = ApiError::Unknown(description.to_string());
            assert!(!is_size_error(&api), "{description}");
        }
    }

    #[test]
    fn media_item_payload_fallback_url_serde_default() {
        // Old queued payloads without the field deserialize with None.
        let json =
            serde_json::json!({"kind": "photo", "media": "https://a/b.jpg", "has_spoiler": false});
        let photo: MediaItemPayload = serde_json::from_value(json).unwrap();
        assert!(matches!(
            photo,
            MediaItemPayload::Photo {
                fallback_url: None,
                ..
            }
        ));
        assert_eq!(photo.fallback_url(), None);
    }

    #[test]
    fn classification_mapping() {
        use teloxide::types::Seconds;
        // RetryAfter -> Retryable with its delay
        let e = RequestError::RetryAfter(Seconds::from_seconds(7));
        assert!(matches!(
            classify_request_error(&e),
            Classification::Retryable { delay_seconds } if delay_seconds == 7.0
        ));
        // Api error -> Permanent
        let e = RequestError::Api(ApiError::Unknown("Bad Request: something".into()));
        assert!(matches!(
            classify_request_error(&e),
            Classification::Permanent { .. }
        ));
        // Api media-fetch marker -> MediaFetchFailure
        let e = RequestError::Api(ApiError::Unknown("Bad Request: WEBPAGE_MEDIA_EMPTY".into()));
        assert!(matches!(
            classify_request_error(&e),
            Classification::MediaFetchFailure
        ));
        // MigrateToChatId -> Permanent
        let e = RequestError::MigrateToChatId(ChatId(123));
        assert!(matches!(
            classify_request_error(&e),
            Classification::Permanent { .. }
        ));
    }

    #[test]
    fn task_serde_round_trip_preserves_resume_state() {
        let task = Task::SendMediaSequence {
            chat_id: 111,
            reply_to_message_id: 222,
            caption: "cap".into(),
            media_batches: vec![
                vec![MediaItemPayload::Photo {
                    media: "https://a/b.jpg".into(),
                    has_spoiler: true,
                    fallback_url: Some("https://a/b_small.jpg".into()),
                    file_id: false,
                }],
                vec![MediaItemPayload::Video {
                    media: "https://a/v.mp4".into(),
                    has_spoiler: false,
                    thumbnail: Some("https://a/t.jpg".into()),
                    fallback_url: None,
                    file_id: false,
                }],
            ],
            batch_index: 1,
            sent_message_ids: vec![11, 12],
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: true,
            forward_channel_id: Some(333),
            notify_chat_id: Some(111),
            notify_message_id: Some(222),
            cache_data: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["type"], "send_media_sequence");
        assert_eq!(json["batch_index"], 1);
        let decoded: Task = serde_json::from_value(json).unwrap();
        match decoded {
            Task::SendMediaSequence {
                batch_index,
                sent_message_ids,
                forward_channel_id,
                media_batches,
                ..
            } => {
                assert_eq!(batch_index, 1);
                assert_eq!(sent_message_ids, vec![11, 12]);
                assert_eq!(forward_channel_id, Some(333));
                assert_eq!(media_batches.len(), 2);
                assert!(matches!(
                    media_batches[0][0],
                    MediaItemPayload::Photo {
                        has_spoiler: true,
                        ..
                    }
                ));
            }
            other => panic!("expected SendMediaSequence, got {other:?}"),
        }
    }

    #[test]
    fn media_item_payload_serde_tags() {
        let photo = MediaItemPayload::Photo {
            media: "https://a/b.jpg".into(),
            has_spoiler: false,
            fallback_url: None,
            file_id: false,
        };
        let json = serde_json::to_value(&photo).unwrap();
        assert_eq!(json["kind"], "photo");
    }

    #[test]
    fn sniff_ext_detects_formats() {
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(sniff_ext(b"\x89PNG\r\n\x1a\n"), "png");
        assert_eq!(sniff_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "webp");
        assert_eq!(sniff_ext(b"GIF89a"), "gif");
        assert_eq!(sniff_ext(b"\x00\x00\x00\x18ftypisom"), "mp4");
        assert_eq!(sniff_ext(b"something else"), "bin");
    }

    // ── MediaSender-mock tests: fallback trigger + error classification ──

    use crate::media_sender::test_support::{MockSender, Outcome};

    /// Telegram's "I could not fetch this URL" error, which triggers the
    /// download-and-reupload fallback.
    fn media_fetch_error() -> RequestError {
        RequestError::Api(ApiError::Unknown("Bad Request: WEBPAGE_MEDIA_EMPTY".into()))
    }

    fn sequence_task(media: &str) -> Task {
        sequence_task_with(media, "cap", None)
    }

    /// A media-group task; `text` (when given) rides in the link-cache
    /// snapshot as `content`, which is where the quote threshold reads it.
    fn sequence_task_with(media: &str, caption: &str, text: Option<&str>) -> Task {
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: caption.into(),
            media_batches: vec![vec![MediaItemPayload::Photo {
                media: media.to_string(),
                has_spoiler: false,
                fallback_url: None,
                file_id: false,
            }]],
            batch_index: 0,
            sent_message_ids: vec![],
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: false,
            forward_channel_id: None,
            notify_chat_id: Some(1),
            notify_message_id: Some(2),
            // The snapshot splits the post's text into title/content the way a
            // real fetch does; the quote threshold joins them again.
            cache_data: text.map(|text| CachedPost {
                url: "https://x.com/u/status/1".into(),
                caption: caption.into(),
                title: String::new(),
                content: text.into(),
                author: "me".into(),
                author_url: "https://x.com/u".into(),
                tags: String::new(),
                sensitive: false,
                media: vec![],
            }),
        }
    }

    /// A media-group task whose built-in caption carries `text` behind the
    /// author link — the shape the quote threshold locates the text in.
    fn sequence_task_with_text(media: &str, text: &str) -> Task {
        let caption =
            format!("https://x.com/u/status/1\n<a href=\"https://x.com/u\">me</a>: {text}");
        sequence_task_with(media, &caption, Some(text))
    }

    #[test]
    fn quote_long_caption_wraps_only_the_text_tail() {
        let text = "一二三四五";
        let prefix = "https://x.com/u/status/1\n<a href=\"https://x.com/u\">me</a>: ";
        let caption = format!("{prefix}{text}");

        // Only the text goes inside the quote; the URL and author line stay
        // outside.
        assert_eq!(
            quote_long_caption(&caption, text, 5),
            format!("{prefix}<blockquote expandable>{text}</blockquote>")
        );
        // One char below the threshold, disabled, and a short text: untouched.
        assert_eq!(
            quote_long_caption(&caption, text, 6),
            format!("{prefix}{text}")
        );
        assert_eq!(quote_long_caption(&caption, text, 0), caption);
        // No author-line anchor means no text to locate — a pixiv caption
        // (title inside the link) and a `{content}`-first format stay as they
        // are rather than risking a blockquote nested in a tag.
        let pixiv =
            format!("<a href=\"https://pixiv.net/1\">{text}</a> / <a href=\"u\">me</a>\ntag");
        assert_eq!(quote_long_caption(&pixiv, text, 5), pixiv);
        let content_first = format!("{text}\nhttps://x.com/u/status/1");
        assert_eq!(quote_long_caption(&content_first, text, 5), content_first);
        // An empty body has nothing to quote.
        assert_eq!(quote_long_caption(prefix, text, 5), prefix);
        // A caption that already carries a blockquote is never nested.
        let quoted = format!("<blockquote>{caption}</blockquote>");
        assert_eq!(quote_long_caption(&quoted, text, 5), quoted);
    }

    /// `truncate_caption` cuts inside the text and appends an ellipsis; the
    /// visible prefix still marks it, so the long-text case that most needs
    /// quoting is still quoted.
    #[test]
    fn quote_long_caption_wraps_a_truncated_text() {
        let text = "一二三四五六七八九十";
        let prefix = "https://x.com/u/status/1\n<a href=\"https://x.com/u\">me</a>: ";
        let caption = format!("{prefix}一二三四五…");
        assert_eq!(
            quote_long_caption(&caption, text, 5),
            format!("{prefix}<blockquote expandable>一二三四五…</blockquote>")
        );
    }

    #[tokio::test]
    async fn long_text_caption_reaches_telegram_quoted() {
        // The threshold is pinned here instead of read from the environment.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.jpg");
        std::fs::write(&file, b"not-a-real-jpeg").unwrap();
        let mut stores = TestStores::new();
        stores.config_mut().caption_quote_text_chars = 5;
        let prefix = "https://x.com/u/status/1\n<a href=\"https://x.com/u\">me</a>: ";

        for (text, expected) in [
            (
                "一二三四五",
                format!("{prefix}<blockquote expandable>一二三四五</blockquote>"),
            ),
            ("一二三四", format!("{prefix}一二三四")),
        ] {
            let sender = MockSender::scripted(vec![Outcome::GroupOk], media_fetch_error);
            let ctx = stores.ctx(&sender);
            let task = sequence_task_with_text(file.to_str().unwrap(), text);
            assert!(send_media_sequence(&ctx, &task).await.is_ok());
            assert_eq!(sender.captions(), vec![expected], "text {text:?}");
        }
    }

    #[tokio::test]
    async fn media_group_fetch_failure_falls_back_then_permanent() {
        // A local file avoids any network in the fallback (the prep pipeline
        // uploads local paths directly). The first group send fails with a
        // media-fetch error → the download-reupload fallback runs → the
        // reupload also fails → Permanent.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.jpg");
        std::fs::write(&file, b"not-a-real-jpeg").unwrap();
        let sender = MockSender::scripted(
            vec![Outcome::GroupErr, Outcome::GroupErr],
            media_fetch_error,
        );
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&ctx, &task).await;
        assert!(
            matches!(result, Err(SendError::Permanent { .. })),
            "got {result:?}"
        );
        // Two group sends: the original + the fallback reupload.
        assert_eq!(sender.calls(), vec!["send_media_group", "send_media_group"]);
    }

    #[tokio::test]
    async fn media_group_retry_after_classifies_retryable_without_fallback() {
        use teloxide::types::Seconds;
        // RetryAfter is not a media-fetch failure: no fallback, straight to a
        // retryable error carrying the Telegram delay.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.jpg");
        std::fs::write(&file, b"not-a-real-jpeg").unwrap();
        let sender = MockSender::scripted(vec![Outcome::GroupErr], || {
            RequestError::RetryAfter(Seconds::from_seconds(7))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&ctx, &task).await;
        match result {
            Err(SendError::Retryable { delay_seconds, .. }) => {
                assert_eq!(delay_seconds, 7.0)
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
        assert_eq!(sender.calls(), vec!["send_media_group"]);
    }

    #[tokio::test]
    async fn animation_fetch_failure_falls_back_then_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gif.mp4");
        std::fs::write(&file, b"not-a-real-mp4").unwrap();
        let sender = MockSender::scripted(
            vec![Outcome::AnimationErr, Outcome::AnimationErr],
            media_fetch_error,
        );
        let task = Task::SendAnimation {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
            animation: MediaItemPayload::Animation {
                media: file.to_string_lossy().into_owned(),
                has_spoiler: false,
                file_id: false,
            },
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: false,
            forward_channel_id: None,
            notify_chat_id: Some(1),
            notify_message_id: Some(2),
            cache_data: None,
        };
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let result = send_animation(&ctx, &task).await;
        assert!(
            matches!(result, Err(SendError::Permanent { .. })),
            "got {result:?}"
        );
        assert_eq!(sender.calls(), vec!["send_animation", "send_animation"]);
    }

    #[tokio::test]
    async fn media_group_success_and_forward_ok() {
        // GroupOk: the group send succeeds (empty message list → no file ids
        // collected, the batch counts as sent). CopyOk: the forward succeeds.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.jpg");
        std::fs::write(&file, b"not-a-real-jpeg").unwrap();
        let sender = MockSender::scripted(vec![Outcome::GroupOk], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&ctx, &task).await;
        assert!(result.is_ok(), "got {result:?}");

        let sender = MockSender::scripted(vec![Outcome::CopyOk], media_fetch_error);
        let ctx = stores.ctx(&sender);
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            notify_chat_id: None,
            notify_message_id: None,
        };
        assert!(forward_messages(&ctx, &task).await.is_ok());
    }

    #[tokio::test]
    async fn forward_classifies_retry_after_and_permanent() {
        use teloxide::types::Seconds;
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            notify_chat_id: None,
            notify_message_id: None,
        };
        // RetryAfter → Retryable with the Telegram delay.
        let sender = MockSender::scripted(vec![Outcome::CopyErr], || {
            RequestError::RetryAfter(Seconds::from_seconds(7))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        match forward_messages(&ctx, &task).await {
            Err(SendError::Retryable { delay_seconds, .. }) => {
                assert_eq!(delay_seconds, 7.0)
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
        // A generic API error → Permanent.
        let sender = MockSender::scripted(vec![Outcome::CopyErr], || {
            RequestError::Api(ApiError::Unknown(
                "Bad Request: message is not modified".into(),
            ))
        });
        let ctx = stores.ctx(&sender);
        assert!(matches!(
            forward_messages(&ctx, &task).await,
            Err(SendError::Permanent { .. })
        ));
    }

    #[tokio::test]
    async fn dead_letter_releases_keep_alive_temp_media() {
        // A task that exhausts its retries is dead-lettered by the queue
        // without the handler running again: the keep-alive temp dir the
        // fetch pipeline handed over must not outlive the task.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ugoira.mp4");
        std::fs::write(&file, b"not-a-real-mp4").unwrap();
        let mut task = sequence_task(file.to_str().unwrap());
        if let Task::SendMediaSequence {
            notify_chat_id,
            notify_message_id,
            ..
        } = &mut task
        {
            // No chat to notify → no Bot is built by the notify path.
            *notify_chat_id = None;
            *notify_message_id = None;
        }
        let dir_path = dir.path().to_path_buf();
        KEEP_ALIVE.lock().push(dir);

        // No chat to notify → the notify path sends nothing (its mock would
        // have no scripted outcome left).
        let sender = MockSender::scripted(vec![Outcome::MessageErr], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let payload = serde_json::to_value(&task).unwrap();
        dead_letter_notify(&ctx, payload, "task failed after 2 retries".into()).await;

        assert!(
            !KEEP_ALIVE.lock().iter().any(|dir| dir.path() == dir_path),
            "dead-lettered task kept its temp media alive"
        );
    }

    #[tokio::test]
    async fn post_send_opens_and_records_the_edit_prompt() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let mut task = sent_task(None, false);
        if let Task::SendMediaSequence {
            edit_before_forward,
            ..
        } = &mut task
        {
            *edit_before_forward = true;
        }

        post_send_actions(&ctx, &task, vec![10, 11]).await;

        assert_eq!(sender.calls(), vec!["send_message"]);
        // The prompt explains the Confirm requirement and the TTL (see the
        // pure `edit_prompt_text` test for the exact wording).
        let prompt_text = sender.messages().first().cloned().unwrap_or_default();
        assert!(prompt_text.contains("Expires in"), "{prompt_text}");
        assert!(prompt_text.contains("Confirm"), "{prompt_text}");
        // The prompt's own message id keys the record the reply will edit.
        let data = stores.chat_store().get(1).await;
        let record = data
            .edit_message
            .get(&MockSender::SENT_ID)
            .expect("the prompt record must be stored");
        assert_eq!(record.url, "https://x.com/u/status/1");
        assert_eq!(record.forward_message_ids, vec![10, 11]);
    }

    #[tokio::test]
    async fn post_send_forwards_immediately_when_configured() {
        let sender = MockSender::scripted(vec![Outcome::CopyOk], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sent_task(Some(2), false);

        post_send_actions(&ctx, &task, vec![10, 11]).await;

        assert_eq!(sender.calls(), vec!["copy_messages"]);
        assert_eq!(stores.queued_tasks().await, 0);
    }

    #[tokio::test]
    async fn post_send_queues_a_retryable_forward() {
        use teloxide::types::Seconds;
        let sender = MockSender::scripted(vec![Outcome::CopyErr], || {
            RequestError::RetryAfter(Seconds::from_seconds(7))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sent_task(Some(2), false);

        post_send_actions(&ctx, &task, vec![10, 11]).await;

        assert_eq!(sender.calls(), vec!["copy_messages"]);
        assert_eq!(stores.queued_tasks().await, 1, "forward retry not queued");
        let payload = stores.queued_payload().await;
        assert_eq!(payload["type"], "forward_messages");
        assert_eq!(payload["to_chat_id"], 2);
        assert_eq!(payload["message_ids"], serde_json::json!([10, 11]));
    }

    #[tokio::test]
    async fn post_send_notifies_a_permanent_forward_failure() {
        let sender = MockSender::scripted(vec![Outcome::CopyErr, Outcome::MessageErr], || {
            RequestError::Api(ApiError::Unknown("Bad Request: chat not found".into()))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = sent_task(Some(2), true);

        post_send_actions(&ctx, &task, vec![10, 11]).await;

        // The copy failed permanently → the chat is told, nothing is queued.
        assert_eq!(sender.calls(), vec!["copy_messages", "send_message"]);
        assert_eq!(stores.queued_tasks().await, 0);
    }

    // ── Settlement: the invariant every terminal path owes ──────────────

    /// An already-sent sequence task with the post-send knobs set: the state
    /// `post_send_actions` branches on.
    fn sent_task(forward_channel_id: Option<i64>, notify: bool) -> Task {
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
            media_batches: vec![vec![MediaItemPayload::Photo {
                media: "https://p/1.jpg".into(),
                has_spoiler: false,
                fallback_url: None,
                file_id: false,
            }]],
            batch_index: 0,
            sent_message_ids: vec![],
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: false,
            forward_channel_id,
            notify_chat_id: notify.then_some(1),
            notify_message_id: notify.then_some(2),
            cache_data: None,
        }
    }

    /// A task whose media are cached Telegram file ids (the only kind that can
    /// hold a link-cache entry).
    fn cached_sequence_task() -> Task {
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
            media_batches: vec![vec![MediaItemPayload::Photo {
                media: "AgAC-file-id".into(),
                has_spoiler: false,
                fallback_url: None,
                file_id: true,
            }]],
            batch_index: 0,
            sent_message_ids: vec![],
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: false,
            forward_channel_id: None,
            notify_chat_id: None,
            notify_message_id: None,
            cache_data: Some(CachedPost {
                url: "https://x.com/u/status/1".into(),
                caption: "cap".into(),
                title: "t".into(),
                content: "c".into(),
                author: "a".into(),
                author_url: "au".into(),
                tags: String::new(),
                sensitive: false,
                media: vec![CachedMedia {
                    kind: CachedMediaKind::Photo,
                    file_id: "AgAC-file-id".into(),
                }],
            }),
        }
    }

    #[tokio::test]
    async fn settled_sent_keeps_the_cache_entry() {
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = cached_sequence_task();
        stores
            .link_cache()
            .put("twitter:1", &cached_sequence_cache_data())
            .await;

        settle_task(&ctx, &task, Settled::Sent).await;

        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_some(),
            "a successful send must not drop its own cache entry"
        );
    }

    #[tokio::test]
    async fn settled_failed_drops_the_cache_entry() {
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = cached_sequence_task();
        stores
            .link_cache()
            .put("twitter:1", &cached_sequence_cache_data())
            .await;

        settle_task(&ctx, &task, Settled::Failed).await;

        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none(),
            "a permanently failed cached send must drop the entry"
        );
    }

    fn cached_sequence_cache_data() -> CachedPost {
        match cached_sequence_task() {
            Task::SendMediaSequence {
                cache_data: Some(post),
                ..
            } => post,
            other => panic!("expected a cached sequence task, got {other:?}"),
        }
    }
}
