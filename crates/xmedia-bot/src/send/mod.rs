//! Task payloads and the send/forward executors, split by concern:
//! [`input_media`] builds Telegram input types from payloads, [`upload`] is
//! the download-and-reupload fallback (Telegram's own fetch of a media URL is
//! blocked by hotlink protection, so the bot downloads the file itself and
//! uploads it via multipart), [`post_send`] covers everything around a send
//! (cache write, keep-alive media, settlement, post-send actions, queue
//! entry points) and [`error`] holds the Bot API error policy. This module
//! keeps the payload types and the senders themselves.

mod error;
mod input_media;
mod post_send;
mod upload;

use crate::ctx::AppContext;
use crate::handlers::log_key;
use crate::link_cache::{CachedMedia, CachedMediaKind, CachedPost};
pub(crate) use error::classify_to_send_error;
pub use error::{
    Classification, SendError, classify_request_error, is_media_fetch_failure, is_size_error,
};
use input_media::{build_media_group, item_url};
use post_send::{cache_animation_send, cache_sent_task};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::LazyLock;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InputMedia, MessageId};
use upload::{PreparedItem, prepare_upload_item, send_batch_via_upload};

// The crate-facing API of this module lives in its submodules; re-export the
// parts other modules use so call sites stay `send::x`.
pub(crate) use post_send::{
    EDIT_PROMPT_EXPIRED_TEXT, KEEP_ALIVE, Settled, dead_letter_notify, enqueue_retry, handle_task,
    notify_failure, post_send_actions, settle_task,
};

/// One process-wide Bot for queue workers. Building a fresh Bot (and its HTTP
/// client) per queue task was pure waste; forced at startup in main so a
/// missing token fails fast instead of on the first task.
pub static BOT: LazyLock<Bot> = LazyLock::new(Bot::from_env);

/// Where an item's bytes come from. One `media: String` used to carry both
/// meanings with a `file_id: bool` beside it to say which — three copies of a
/// flag every reader had to re-check.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaRef {
    /// A media URL Telegram fetches itself, or a local path to upload.
    Source(String),
    /// A Telegram file id from the link cache: sent as-is, no fetch, no upload.
    FileId(String),
}

/// A queued retry persists this payload, so its wire shape is a contract with
/// the rows already on disk: `media` used to be a bare string with a
/// `file_id: bool` beside it, and a row in that older shape no longer parses —
/// the queue dead-letters it (`handle_task`'s "invalid task payload") and the
/// dead-letter path still names the post, so the one-time upgrade cost is a
/// retry that could not be resumed anyway.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaItemPayload {
    Photo {
        media: MediaRef,
        has_spoiler: bool,
        /// Smaller variant used when the primary media exceeds Telegram's
        /// size limits.
        #[serde(default)]
        fallback_url: Option<String>,
    },
    Video {
        media: MediaRef,
        has_spoiler: bool,
        thumbnail: Option<String>,
        #[serde(default)]
        fallback_url: Option<String>,
    },
    Animation {
        media: MediaRef,
        has_spoiler: bool,
    },
}

impl MediaItemPayload {
    /// The item's media reference, whatever kind of item it is.
    pub(crate) fn media_ref(&self) -> &MediaRef {
        match self {
            MediaItemPayload::Photo { media, .. }
            | MediaItemPayload::Video { media, .. }
            | MediaItemPayload::Animation { media, .. } => media,
        }
    }

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
        #[serde(default)]
        forward_offset: usize,
        notify_chat_id: Option<i64>,
        notify_message_id: Option<i64>,
    },
}

/// The delivery envelope of a task built from ready-made items: who receives
/// the media, where a failure notice goes, and whether the chat's
/// edit-before-forward / channel-forward settings apply. The fresh-send path
/// fills it from the chat's settings, the startup repair from the queued row it
/// replaces, so the two shapes cannot drift apart.
pub(crate) struct Delivery {
    pub chat_id: i64,
    pub reply_to_message_id: i64,
    pub edit_before_forward: bool,
    pub forward_channel_id: Option<i64>,
    pub notify_chat_id: Option<i64>,
    pub notify_message_id: Option<i64>,
}

impl Task {
    /// A send task for ready-made `items`: a lone animation takes the
    /// SendAnimation path, everything else the media sequence (photos first,
    /// chunked). The one place that shape is written.
    pub(crate) fn from_items(
        delivery: Delivery,
        source_url: String,
        caption: String,
        items: Vec<MediaItemPayload>,
        cache_data: Option<CachedPost>,
    ) -> Task {
        let Delivery {
            chat_id,
            reply_to_message_id,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
        } = delivery;
        if items.len() == 1 && matches!(items[0], MediaItemPayload::Animation { .. }) {
            Task::SendAnimation {
                chat_id,
                reply_to_message_id,
                caption,
                animation: items.into_iter().next().unwrap(),
                source_url,
                edit_before_forward,
                forward_channel_id,
                notify_chat_id,
                notify_message_id,
                cache_data,
            }
        } else {
            Task::SendMediaSequence {
                chat_id,
                reply_to_message_id,
                caption,
                // Photos first so a mixed photo+video group starts with a photo
                // (Telegram's sendMediaGroup rule); order within each kind is kept.
                media_batches: chunk_media_items(photos_first(items)),
                // A fresh delivery: nothing of this payload has been sent.
                batch_index: 0,
                sent_message_ids: vec![],
                source_url,
                edit_before_forward,
                forward_channel_id,
                notify_chat_id,
                notify_message_id,
                cache_data,
            }
        }
    }

    fn cache_data(&self) -> Option<&CachedPost> {
        match self {
            Task::SendMediaSequence { cache_data, .. } | Task::SendAnimation { cache_data, .. } => {
                cache_data.as_ref()
            }
            Task::ForwardMessages { .. } => None,
        }
    }

    pub(crate) fn source_url(&self) -> Option<&str> {
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

    /// The chat this task delivers media to (`None` for a channel copy, which
    /// names two chats instead).
    pub(crate) fn chat_id(&self) -> Option<i64> {
        match self {
            Task::SendMediaSequence { chat_id, .. } | Task::SendAnimation { chat_id, .. } => {
                Some(*chat_id)
            }
            Task::ForwardMessages { .. } => None,
        }
    }

    /// Where a failure notice for this task goes (both `None` for a copy with
    /// nothing to notify).
    pub(crate) fn notify_target(&self) -> (Option<i64>, Option<i64>) {
        match self {
            Task::SendMediaSequence {
                notify_chat_id,
                notify_message_id,
                ..
            }
            | Task::SendAnimation {
                notify_chat_id,
                notify_message_id,
                ..
            }
            | Task::ForwardMessages {
                notify_chat_id,
                notify_message_id,
                ..
            } => (*notify_chat_id, *notify_message_id),
        }
    }

    /// Local file paths referenced by this task's media (ugoira / bsky remux
    /// MP4 and the like); empty for URL or Telegram file-id sends.
    pub(crate) fn local_media_paths(&self) -> Vec<std::path::PathBuf> {
        // The task's items: sequence batches flattened, or the lone animation.
        let items: Vec<&MediaItemPayload> = match self {
            Task::SendMediaSequence { media_batches, .. } => {
                media_batches.iter().flatten().collect()
            }
            Task::SendAnimation { animation, .. } => {
                std::slice::from_ref(animation).iter().collect()
            }
            Task::ForwardMessages { .. } => Vec::new(),
        };
        let mut out = Vec::new();
        for item in items {
            // A file id names a Telegram-hosted copy, not a local file.
            if matches!(item.media_ref(), MediaRef::FileId(_)) {
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

/// A cache entry may replay a remote source URL. Local temp paths disappear
/// when the task settles and must never be persisted as a source.
pub(super) fn replayable_cache_url(media: &str) -> String {
    if media.starts_with("http://") || media.starts_with("https://") {
        media.to_string()
    } else {
        String::new()
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
                url: replayable_cache_url(item_url(item)),
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
pub fn photos_first(mut items: Vec<MediaItemPayload>) -> Vec<MediaItemPayload> {
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
                log::warn!(
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
        MediaItemPayload::Animation { has_spoiler, .. } => (item_url(animation), *has_spoiler),
        MediaItemPayload::Photo { .. } | MediaItemPayload::Video { .. } => {
            unreachable!("SendAnimation carries an Animation payload")
        }
    };
    // The payload knows whether its media is a URL/path or a cached file id
    // (this used to go through `input_file_for`, which treated a file id as a
    // local path and answered "local media file missing").
    let url_file = match animation.input_file() {
        Ok(file) => file,
        Err(message) => {
            return Err(SendError::Permanent {
                message,
                task: Box::new(task.clone()),
            });
        }
    };
    match ctx
        .sender
        .send_animation(
            ChatId(chat_id),
            MessageId(reply_to as i32),
            &caption,
            has_spoiler,
            url_file,
        )
        .await
    {
        Ok(message) => {
            let id = message.id.0 as i64;
            cache_animation_send(ctx, task, &message, media_url).await;
            Ok(vec![id])
        }
        Err(RequestError::Api(api)) if is_media_fetch_failure(&api) || is_size_error(&api) => {
            log::warn!(
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
                    match ctx
                        .sender
                        .send_animation(
                            ChatId(chat_id),
                            MessageId(reply_to as i32),
                            &caption,
                            has_spoiler,
                            animation.media,
                        )
                        .await
                    {
                        Ok(message) => {
                            let id = message.id.0 as i64;
                            cache_animation_send(ctx, task, &message, media_url).await;
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

/// Sends a post that has no media of its own: its caption — the post's URL,
/// author and text, in the chat's per-site format, long-post quoting included
/// — becomes the message. Such a post used to be answered with "No media
/// found", throwing away text the fetch had already parsed and escaped.
///
/// No queue entry: there is no `Task` shape for text, and a post with nothing
/// to download is cheap for the user to paste again, so a failure is reported
/// rather than retried.
pub(crate) async fn send_text_post(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to: i64,
    caption: String,
) {
    match ctx
        .sender
        .send_html_message(ChatId(chat_id), caption, Some(MessageId(reply_to as i32)))
        .await
    {
        Ok(_) => log::info!("sent the post's text for chat={chat_id}"),
        Err(e) => {
            log::warn!("could not send the post's text for chat={chat_id}: {e}");
            notify_failure(
                ctx.sender,
                Some(chat_id),
                Some(reply_to),
                "Could not send this post's text.",
            )
            .await;
        }
    }
}

pub async fn forward_messages(ctx: &AppContext<'_>, task: &Task) -> Result<(), SendError> {
    let Task::ForwardMessages {
        from_chat_id,
        to_chat_id,
        message_ids,
        forward_offset,
        ..
    } = task
    else {
        unreachable!("forward_messages requires a ForwardMessages task")
    };
    let mut offset = (*forward_offset).min(message_ids.len());
    while offset < message_ids.len() {
        let end = (offset + 100).min(message_ids.len());
        let ids = message_ids[offset..end]
            .iter()
            .copied()
            .map(|id| MessageId(id as i32))
            .collect();
        if let Err(e) = ctx
            .sender
            .copy_messages(ChatId(*to_chat_id), ChatId(*from_chat_id), ids)
            .await
        {
            let mut retry_task = task.clone();
            if let Task::ForwardMessages { forward_offset, .. } = &mut retry_task {
                *forward_offset = offset;
            }
            return Err(classify_to_send_error(&e, retry_task, "media fetch failed"));
        }
        offset = end;
    }
    log::info!(
        "copied {} message(s) from {} to {}",
        message_ids.len(),
        from_chat_id,
        to_chat_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::post_send::{build_edit_markup, cache_sent_task};
    use super::upload::sniff_ext;
    use super::*;
    use crate::ctx::test_support::{TestStores, api_error, cached_photo, photo_item};
    use std::collections::HashMap;
    use std::time::Duration;
    use teloxide::ApiError;

    /// The two multipart upload caps, pinned where the bot draws them: photos
    /// are the 10 MiB case, everything else the 50 MB one. A single cap for
    /// both refused to download a 10–50 MB video that Telegram would have
    /// accepted (and a video has no smaller variant to fall back to).
    #[test]
    fn upload_caps_match_telegrams_limits() {
        const { assert!(crate::photo::MAX_UPLOAD_BYTES == 10 * 1024 * 1024) };
        const { assert!(super::upload::MAX_MEDIA_UPLOAD_BYTES == 50 * 1024 * 1024) };
        const { assert!(crate::photo::MAX_PHOTO_DOWNLOAD_BYTES <= crate::photo::MAX_DECODE_BYTES) };
    }

    #[test]
    fn oversized_photo_boundary() {
        // The empirical Telegram limit: sum 10000 passes, 10001 fails. Pinned
        // cross-crate because `photo.rs` and `upload.rs` both branch on it.
        // Const-block asserts so clippy's assertions_on_constants stays quiet.
        const { assert!(crate::photo::PHOTO_MAX_DIMENSION_SUM == 10000) };
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
        use MediaItemPayload::{Photo, Video};
        let photo = |u: &str| Photo {
            media: MediaRef::Source(u.into()),
            has_spoiler: false,
            fallback_url: None,
        };
        let video = |u: &str| Video {
            media: MediaRef::Source(u.into()),
            has_spoiler: false,
            thumbnail: None,
            fallback_url: None,
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
            .map(|i| match i.media_ref() {
                MediaRef::Source(media) | MediaRef::FileId(media) => media.as_str(),
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

    /// A media-less post goes out as a message; a failure is reported rather
    /// than swallowed (there is no task to retry).
    #[tokio::test]
    async fn a_text_post_is_sent_or_reported() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);

        send_text_post(
            &ctx,
            1,
            2,
            "https://x.com/u/status/1\n<a>u</a>: hello".to_string(),
        )
        .await;

        assert_eq!(sender.calls(), vec!["send_html_message"]);
        assert_eq!(
            sender.messages(),
            vec!["https://x.com/u/status/1\n<a>u</a>: hello"]
        );

        // The failure path: the send fails, the notice follows.
        let sender = MockSender::scripted(vec![Outcome::MessageErr, Outcome::MessageOk], || {
            api_error("Bad Request: chat not found")
        });
        let ctx = stores.ctx(&sender);

        send_text_post(&ctx, 1, 2, "text".into()).await;

        assert_eq!(sender.calls(), vec!["send_html_message", "send_message"]);
        assert!(sender.messages()[1].contains("Could not send this post's text"));
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
    fn edit_markup_folds_and_caps_the_template_buttons() {
        // Telegram rejects a keyboard over 100 buttons, which would drop the
        // whole prompt; the cap keeps it well under that.
        let templates: HashMap<String, String> = (0..200)
            .map(|i| (format!("t{i:03}"), "[]".to_string()))
            .collect();
        let keyboard = build_edit_markup(&templates);
        let buttons: usize = keyboard.inline_keyboard.iter().map(Vec::len).sum();
        assert!(
            buttons <= 100,
            "a keyboard Telegram rejects would lose the prompt: {buttons}"
        );
        assert_eq!(
            buttons,
            super::post_send::MAX_TEMPLATE_BUTTONS + 2,
            "the cap plus the confirm/skip pair"
        );
        // Names are folded, not one per row.
        assert_eq!(keyboard.inline_keyboard[0].len(), 3);
        assert_eq!(keyboard.inline_keyboard.last().unwrap().len(), 2);
        assert_eq!(
            templates
                .len()
                .saturating_sub(super::post_send::MAX_TEMPLATE_BUTTONS),
            140
        );
        // Under the cap nothing is hidden and every name gets a button.
        let few: HashMap<String, String> = (0..4)
            .map(|i| (format!("t{i}"), "[]".to_string()))
            .collect();
        assert_eq!(
            few.len()
                .saturating_sub(super::post_send::MAX_TEMPLATE_BUTTONS),
            0
        );
        assert_eq!(
            build_edit_markup(&few)
                .inline_keyboard
                .iter()
                .map(Vec::len)
                .sum::<usize>(),
            6
        );
    }

    #[test]
    fn failure_text_names_the_post_and_the_cause() {
        // A send failure names the post (the cache key) and the cause, so the
        // user knows which of their links died.
        let task = sequence_task("https://x.com/u/status/1");
        let text = super::post_send::failure_text(task.source_url(), "retries exhausted");
        assert!(text.contains("twitter:1"), "{text}");
        assert!(text.contains("retries exhausted"), "{text}");

        // A channel-forward failure has no source URL: it must not claim a
        // post failed.
        let forward = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![1],
            forward_offset: 0,
            notify_chat_id: None,
            notify_message_id: None,
        };
        let text = super::post_send::failure_text(forward.source_url(), "chat not found");
        assert!(text.starts_with("Forward failed permanently"), "{text}");
        assert!(text.contains("chat not found"), "{text}");
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
    fn is_media_fetch_failure_matches_the_single_media_url_description() {
        // `sendPhoto`/`sendAnimation`-style URL sends answer with this one
        // instead of the `webpage_*` markers; without it the URL send failed
        // permanently instead of going through the reupload fallback.
        let api = ApiError::Unknown("Bad Request: failed to get HTTP URL content".into());
        assert!(is_media_fetch_failure(&api));
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
        // `fallback_url` is optional in the wire shape: a payload written
        // without it deserializes with `None`.
        let json = serde_json::json!({
            "kind": "photo",
            "media": {"source": "https://a/b.jpg"},
            "has_spoiler": false,
        });
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
        // A server-error description (teloxide drops the HTTP status, so a
        // JSON 5xx arrives as an unknown description) -> Retryable. Without
        // this a Telegram 502 dead-lettered the post.
        let e = RequestError::Api(ApiError::Unknown("Internal Server Error".into()));
        assert!(matches!(
            classify_request_error(&e),
            Classification::Retryable { .. }
        ));
        // An HTML/proxy error page in place of the API's JSON -> Retryable.
        let e = RequestError::InvalidJson {
            source: std::sync::Arc::new(
                serde_json::from_str::<serde_json::Value>("<html>502</html>").unwrap_err(),
            ),
            raw: "<html>502 Bad Gateway</html>".into(),
        };
        assert!(matches!(
            classify_request_error(&e),
            Classification::Retryable { .. }
        ));
        // A JSON body of the wrong shape is a type mismatch, not a transport
        // problem: still permanent.
        // (The `source` is only ever rendered, so an unrelated parse error
        // stands in for the shape mismatch; `raw` is what the classifier reads.)
        let e = RequestError::InvalidJson {
            source: std::sync::Arc::new(
                serde_json::from_str::<serde_json::Value>("x").unwrap_err(),
            ),
            raw: "{\"ok\":true,\"result\":true}".into(),
        };
        assert!(matches!(
            classify_request_error(&e),
            Classification::Permanent { .. }
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
                    media: MediaRef::Source("https://a/b.jpg".into()),
                    has_spoiler: true,
                    fallback_url: Some("https://a/b_small.jpg".into()),
                }],
                vec![MediaItemPayload::Video {
                    media: MediaRef::Source("https://a/v.mp4".into()),
                    has_spoiler: false,
                    thumbnail: Some("https://a/t.jpg".into()),
                    fallback_url: None,
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
        let photo = photo_item("https://a/b.jpg", false, false);
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
            media_batches: vec![vec![photo_item(media, false, false)]],
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
                media: MediaRef::Source(file.to_string_lossy().into_owned()),
                has_spoiler: false,
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

    /// A media group through a **real** `Bot` — its request building, the
    /// per-chat limiter, the bot-wide budget — against a stand-in API. The
    /// scripted mock bypasses `media_sender`'s implementation entirely, so a
    /// call site that stops charging the limiters (or a broken request shape)
    /// is invisible to every other test.
    #[tokio::test]
    async fn a_media_group_reaches_the_api_through_a_real_bot() {
        use crate::media_sender::test_support::fake_api::FakeApi;
        use teloxide::Bot;

        let api = FakeApi::start().await;
        let bot = Bot::new("42:TEST").set_api_url(api.url());
        let stores = TestStores::new();
        let ctx = stores.ctx(&bot);
        // A chat of its own: the limiter buckets are process-wide.
        let mut task = sequence_task("https://cdn.example/1.jpg");
        if let Task::SendMediaSequence { chat_id, .. } = &mut task {
            *chat_id = 987_654;
        }
        let bucket = crate::rate_limit::limiter_for(987_654);
        let before = bucket.tokens();

        let outcome = send_media_sequence(&ctx, &task).await;
        eprintln!(
            "SCRATCH send methods={:?} outcome={outcome:?}",
            api.methods()
        );
        assert!(outcome.is_ok());

        // The request teloxide built: one group, the URL, the caption on the
        // first item.
        assert_eq!(api.methods(), vec!["SendMediaGroup"]);
        let body = api.body("SendMediaGroup");
        assert_eq!(body["chat_id"], 987_654);
        assert_eq!(body["media"][0]["media"], "https://cdn.example/1.jpg");
        assert_eq!(body["media"][0]["caption"], "cap");
        // …and the send charged the pace limiter before it went out.
        let after = bucket.tokens();
        assert!(
            after < before,
            "a send must charge the chat's budget ({before} -> {after})"
        );
    }

    #[tokio::test]
    async fn forward_classifies_retry_after_and_permanent() {
        use teloxide::types::Seconds;
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            forward_offset: 0,
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
    async fn long_forward_is_split_into_telegram_batches() {
        let sender = MockSender::scripted(vec![Outcome::CopyOk, Outcome::CopyOk], || {
            api_error("unused")
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: (1..=201).collect(),
            forward_offset: 0,
            notify_chat_id: None,
            notify_message_id: None,
        };
        forward_messages(&ctx, &task).await.unwrap();
        assert_eq!(
            sender.calls(),
            vec!["copy_messages", "copy_messages", "copy_messages"]
        );
    }

    #[tokio::test]
    async fn failed_forward_resumes_after_completed_batches() {
        use teloxide::types::Seconds;
        let sender = MockSender::scripted(vec![Outcome::CopyOk, Outcome::CopyErr], || {
            RequestError::RetryAfter(Seconds::from_seconds(7))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: (1..=201).collect(),
            forward_offset: 0,
            notify_chat_id: None,
            notify_message_id: None,
        };

        match forward_messages(&ctx, &task).await {
            Err(SendError::Retryable { task, .. }) => {
                let Task::ForwardMessages { forward_offset, .. } = *task else {
                    panic!("retry task is not a forward");
                };
                assert_eq!(forward_offset, 100);
            }
            other => panic!("expected retryable forward, got {other:?}"),
        }
        assert_eq!(
            sender.calls(),
            vec!["copy_messages", "copy_messages"],
            "the first completed batch must not be replayed"
        );
    }

    #[tokio::test]
    async fn dead_letter_releases_keep_alive_temp_media() {
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
        KEEP_ALIVE.lock().push(std::sync::Arc::new(dir));

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
    async fn one_settle_drops_only_one_shared_keep_alive_holder() {
        // Two pipelines of the same post (shared fetch) push the same temp
        // dir once each. One task settling must drop only its own reference —
        // clearing every holder would delete the file out from under the
        // other task's queued retry, which would then dead-letter on a local
        // media that no longer exists.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ugoira.mp4");
        std::fs::write(&file, b"not-a-real-mp4").unwrap();
        let task = sequence_task(file.to_str().unwrap());
        let dir_path = dir.path().to_path_buf();
        let dir = std::sync::Arc::new(dir);
        {
            let mut alive = KEEP_ALIVE.lock();
            alive.push(std::sync::Arc::clone(&dir));
            alive.push(std::sync::Arc::clone(&dir));
        }

        // `Settled::Sent` never talks to the API, so no outcome is scripted.
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        settle_task(&ctx, &task, Settled::Sent).await;

        let holders = KEEP_ALIVE
            .lock()
            .iter()
            .filter(|d| d.path() == dir_path)
            .count();
        // Drop this test's entries so the registry does not outlive it.
        KEEP_ALIVE.lock().retain(|d| d.path() != dir_path);
        assert_eq!(holders, 1, "one settle must drop exactly one shared holder");
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
            media_batches: vec![vec![photo_item("https://p/1.jpg", false, false)]],
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
            media_batches: vec![vec![photo_item("AgAC-file-id", false, true)]],
            batch_index: 0,
            sent_message_ids: vec![],
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: false,
            forward_channel_id: None,
            notify_chat_id: None,
            notify_message_id: None,
            cache_data: Some(cached_photo()),
        }
    }

    #[tokio::test]
    async fn a_degraded_entry_regains_the_file_ids_a_send_produced() {
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        // A degraded entry: no file ids, URL only (what `invalidate_cache`
        // leaves behind).
        let mut degraded = cached_photo();
        degraded.media[0].file_id.clear();
        stores.link_cache().put("twitter:1", &degraded).await;
        let mut task = cached_sequence_task();
        if let Task::SendMediaSequence { cache_data, .. } = &mut task {
            *cache_data = Some(degraded.clone());
        }

        cache_sent_task(
            &ctx,
            &task,
            vec![CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: "fresh-id".into(),
                url: "https://pbs.twimg.com/media/photo.jpg".into(),
            }],
        )
        .await;

        let entry = stores
            .link_cache()
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .expect("the entry must still be there");
        assert_eq!(
            entry.media[0].file_id, "fresh-id",
            "a degraded entry must take the ids its send produced"
        );

        // A send served from a healthy entry must not rewrite it: the ids it
        // already holds are exactly what the next repeat wants. Which of the
        // two a send was is the *task's* cache snapshot — a healthy one carries
        // file ids.
        cache_sent_task(
            &ctx,
            &cached_sequence_task(),
            vec![CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: "other-id".into(),
                url: String::new(),
            }],
        )
        .await;
        let entry = stores
            .link_cache()
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(
            entry.media[0].file_id, "fresh-id",
            "a healthy entry is left alone"
        );
    }

    #[tokio::test]
    async fn settled_sent_keeps_the_cache_entry() {
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = cached_sequence_task();
        stores.link_cache().put("twitter:1", &cached_photo()).await;

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
    async fn settled_failed_degrades_the_cache_entry_then_drops_it() {
        let sender = MockSender::scripted(vec![], media_fetch_error);
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let task = cached_sequence_task();
        stores.link_cache().put("twitter:1", &cached_photo()).await;

        settle_task(&ctx, &task, Settled::Failed).await;

        // The file id is what failed, not the media: the entry survives with
        // its source URLs, so the next request re-sends without a fetch.
        let entry = stores
            .link_cache()
            .get("twitter:1", Duration::from_secs(3600))
            .await
            .expect("a failed cached send must not drop the entry outright");
        assert_eq!(entry.media.len(), 1);
        assert!(entry.media[0].file_id.is_empty(), "the stale id must go");
        assert_eq!(entry.media[0].url, "https://pbs.twimg.com/media/photo.jpg");
        assert_eq!(entry.caption, "cap", "the text is still good");

        // A second failure — this time the URLs did not work either — drops it.
        settle_task(&ctx, &task, Settled::Failed).await;
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(3600))
                .await
                .is_none(),
            "a degraded entry that fails again must be dropped"
        );
    }
    #[test]
    fn local_media_cache_does_not_store_a_dead_path() {
        assert_eq!(replayable_cache_url("/tmp/tgxmb-ugoira/video.mp4"), "");
        assert_eq!(
            replayable_cache_url("https://cdn.example/video.mp4"),
            "https://cdn.example/video.mp4"
        );
    }
}
