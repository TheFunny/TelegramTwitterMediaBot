//! Typed task payloads and send/forward executors with retry classification
//! and the download-and-reupload fallback (Telegram's own fetch of a media
//! URL is blocked by hotlink protection; the bot downloads the file itself
//! and uploads it via multipart).

use crate::handlers::{CHAT_STORE, LINK_CACHE, TASK_QUEUE};
use crate::link_cache::{CachedMedia, CachedMediaKind, CachedPost};
use crate::photo::{self, MAX_UPLOAD_BYTES, PhotoPrep};
use crate::queue::QueueError;
use crate::state::{EditMessage, unix_now};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaAnimation,
    InputMediaPhoto, InputMediaVideo, Message, MessageId, ParseMode, ReplyParameters,
};
use teloxide::{ApiError, RequestError};
use tempfile::NamedTempFile;
use x_media::site::FetchError;

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

/// Persists a successful send under the post's cache key. Only runs for a
/// fresh (non-resumed) task that carried raw cache data with no file ids yet.
async fn cache_sent_task(task: &Task, media: Vec<CachedMedia>) {
    let Some(cache_data) = task.cache_data() else {
        return;
    };
    if !cache_data.media.is_empty() || media.is_empty() {
        return;
    }
    let mut post = cache_data.clone();
    post.media = media;
    if let Some(key) = x_media::site::cache_key(&post.url) {
        LINK_CACHE.put(&key, &post).await;
        log::info!("cached send for {}", post.url);
    }
}

/// Persists a lone animation send under the post's cache key.
async fn cache_animation_send(task: &Task, message: &Message) {
    if let Some(file_id) = message.animation().map(|a| a.file.id.to_string()) {
        cache_sent_task(
            task,
            vec![CachedMedia {
                kind: CachedMediaKind::Animation,
                file_id,
            }],
        )
        .await;
    }
}

/// A cached Telegram file id failed permanently (stale/expired); drop the
/// cache entry so the next request re-fetches instead of repeating it.
pub async fn invalidate_cache(task: &Task) {
    if task.is_cached_send()
        && let Some(url) = task.source_url()
        && let Some(key) = x_media::site::cache_key(url)
    {
        log::info!("removing stale link cache entry for {url}");
        LINK_CACHE.remove(&key).await;
    }
}

pub const MAX_MEDIA_GROUP: usize = 9;

/// Splits media into batches of at most [`MAX_MEDIA_GROUP`] items.
pub fn chunk_media_items<T: Clone>(items: Vec<T>) -> Vec<Vec<T>> {
    items
        .chunks(MAX_MEDIA_GROUP)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// Exponential backoff with jitter, capped at 30s.
pub fn retry_delay_seconds(attempts: u32) -> f64 {
    let jitter: f64 = rand::thread_rng().gen_range(0.2..0.8);
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
        "PHOTO_INVALID_DIMENSIONS",
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

pub enum SendError {
    Retryable { delay_seconds: f64, task: Task },
    Permanent { message: String, task: Task },
}

fn classify_to_send_error(e: &RequestError, task: Task) -> SendError {
    match classify_request_error(e) {
        Classification::Retryable { delay_seconds } => SendError::Retryable {
            delay_seconds,
            task,
        },
        Classification::Permanent { message } => SendError::Permanent { message, task },
        Classification::MediaFetchFailure => SendError::Permanent {
            message: "media fetch failed".into(),
            task,
        },
    }
}

fn parse_media_url(s: &str) -> Result<url::Url, String> {
    url::Url::parse(s).map_err(|e| format!("invalid media URL: {e}"))
}

fn item_url(item: &MediaItemPayload) -> &str {
    match item {
        MediaItemPayload::Photo { media, .. }
        | MediaItemPayload::Video { media, .. }
        | MediaItemPayload::Animation { media, .. } => media,
    }
}

/// Remote http(s) URLs are handed to Telegram to fetch; everything else
/// (e.g. a locally encoded ugoira MP4) is uploaded directly.
fn input_file_for(media: &str) -> Result<InputFile, String> {
    if media.starts_with("http://") || media.starts_with("https://") {
        Ok(InputFile::url(parse_media_url(media)?))
    } else if !std::path::Path::new(media).exists() {
        // A retried task may reference a temp file the original send's
        // TempDir already cleaned up; fail fast and permanent instead of
        // burning retries on a file that can never come back.
        Err(format!("local media file missing: {media}"))
    } else {
        Ok(InputFile::file(media))
    }
}

impl MediaItemPayload {
    /// The input for a send: a cached file id goes out as `InputFile::file_id`
    /// (no fetch, no upload), URLs go to Telegram, anything else is a local
    /// path (transient upload fallback).
    fn input_file(&self) -> Result<InputFile, String> {
        match self {
            MediaItemPayload::Photo {
                media,
                file_id: true,
                ..
            }
            | MediaItemPayload::Video {
                media,
                file_id: true,
                ..
            }
            | MediaItemPayload::Animation {
                media,
                file_id: true,
                ..
            } => Ok(InputFile::file_id(media.clone().into())),
            _ => input_file_for(item_url(self)),
        }
    }
}

fn photo_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut photo = InputMediaPhoto::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        photo = photo.caption(caption);
    }
    if spoiler {
        photo = photo.spoiler();
    }
    InputMedia::Photo(photo)
}

fn video_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut video = InputMediaVideo::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        video = video.caption(caption);
    }
    if spoiler {
        video = video.spoiler();
    }
    InputMedia::Video(video)
}

fn animation_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut animation = InputMediaAnimation::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        animation = animation.caption(caption);
    }
    if spoiler {
        animation = animation.spoiler();
    }
    InputMedia::Animation(animation)
}

/// Builds a media group from payloads; only the first item of the batch gets
/// the caption (Telegram rejects captions on later items).
fn build_media_group(
    batch: &[MediaItemPayload],
    caption: Option<&str>,
) -> Result<Vec<InputMedia>, String> {
    batch
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let item_caption = if i == 0 { caption } else { None };
            Ok(match item {
                MediaItemPayload::Photo { has_spoiler, .. } => {
                    photo_media(item.input_file()?, item_caption, *has_spoiler)
                }
                MediaItemPayload::Video {
                    has_spoiler,
                    thumbnail,
                    ..
                } => {
                    let mut video = video_media(item.input_file()?, item_caption, *has_spoiler);
                    if let (Some(thumb), InputMedia::Video(v)) = (thumbnail, &mut video) {
                        *v = v.clone().thumbnail(input_file_for(thumb)?);
                    }
                    video
                }
                MediaItemPayload::Animation { has_spoiler, .. } => {
                    animation_media(item.input_file()?, item_caption, *has_spoiler)
                }
            })
        })
        .collect()
}

/// Infers a file extension from magic bytes so Telegram detects the mime type
/// on multipart uploads.
fn sniff_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        "mp4"
    } else {
        "bin"
    }
}

enum FallbackError {
    Retryable {
        delay_seconds: f64,
    },
    Permanent {
        message: String,
    },
    /// The downloaded file exceeds the upload cap; the caller falls back to
    /// the item's smaller URL.
    MediaTooLarge,
}

/// Brings a downloaded photo within Telegram's limits via the pure-Rust
/// chain in [`crate::photo`] (no ffmpeg): dimension cap / upload cap
/// exceeded photos are decoded, downscaled with Lanczos3, PNG bit depth
/// reduced (>24-bit → 24-bit RGB, ≤24-bit untouched) and transcoded to JPEG
/// only if still too big. Anything that cannot be fixed falls back to the
/// item's smaller URL.
///
/// Downloads one media item to a temp file (deleted on drop). Network errors
/// are retryable; size over the upload cap and other download errors are not.
async fn download_to_temp(item: &MediaItemPayload) -> Result<NamedTempFile, FallbackError> {
    let media_url = match item {
        MediaItemPayload::Photo { media, .. }
        | MediaItemPayload::Video { media, .. }
        | MediaItemPayload::Animation { media, .. } => media,
    };
    // Photos are downloaded even over the upload cap so `prepare_photo` can
    // downscale / transcode them (cap = decode budget); videos/animations
    // abort as soon as the upload cap is crossed mid-stream.
    let limit = if matches!(item, MediaItemPayload::Photo { .. }) {
        photo::MAX_DECODE_BYTES
    } else {
        MAX_UPLOAD_BYTES + 1
    };
    let bytes = match x_media::site::download_media_limited(media_url, limit).await {
        Ok(bytes) => bytes,
        Err(FetchError::Http(_)) => {
            return Err(FallbackError::Retryable {
                delay_seconds: retry_delay_seconds(0),
            });
        }
        Err(FetchError::TooLarge) => {
            return Err(FallbackError::MediaTooLarge);
        }
        Err(e) => {
            return Err(FallbackError::Permanent {
                message: format!("download failed: {e}"),
            });
        }
    };
    let ext = sniff_ext(&bytes);
    let mut file = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .map_err(|e| FallbackError::Permanent {
            message: format!("temp file failed: {e}"),
        })?;
    use std::io::Write;
    file.as_file_mut()
        .write_all(&bytes)
        .map_err(|e| FallbackError::Permanent {
            message: format!("temp file write failed: {e}"),
        })?;
    Ok(file)
}

/// Builds the media group item from an uploaded file.
fn media_from_file(
    item: &MediaItemPayload,
    path: std::path::PathBuf,
    caption: Option<&str>,
    thumbnail: Option<&str>,
) -> Result<InputMedia, String> {
    let mut media = match item {
        MediaItemPayload::Photo { has_spoiler, .. } => {
            photo_media(InputFile::file(path), caption, *has_spoiler)
        }
        MediaItemPayload::Video { has_spoiler, .. } => {
            video_media(InputFile::file(path), caption, *has_spoiler)
        }
        MediaItemPayload::Animation { has_spoiler, .. } => {
            animation_media(InputFile::file(path), caption, *has_spoiler)
        }
    };
    if let (Some(thumb), InputMedia::Video(v)) = (thumbnail, &mut media) {
        *v = v.clone().thumbnail(input_file_for(thumb)?);
    }
    Ok(media)
}

/// Builds the media group item from a (smaller) URL.
fn media_from_url(
    item: &MediaItemPayload,
    url: &str,
    caption: Option<&str>,
    thumbnail: Option<&str>,
) -> Result<InputMedia, String> {
    let mut media = match item {
        MediaItemPayload::Photo { has_spoiler, .. } => {
            photo_media(input_file_for(url)?, caption, *has_spoiler)
        }
        MediaItemPayload::Video { has_spoiler, .. } => {
            video_media(input_file_for(url)?, caption, *has_spoiler)
        }
        MediaItemPayload::Animation { has_spoiler, .. } => {
            animation_media(input_file_for(url)?, caption, *has_spoiler)
        }
    };
    if let (Some(thumb), InputMedia::Video(v)) = (thumbnail, &mut media) {
        *v = v.clone().thumbnail(input_file_for(thumb)?);
    }
    Ok(media)
}

/// Download-and-reupload fallback for one media batch. Files over the upload
/// cap are not downloaded/uploaded; the item falls back to its smaller URL
/// (which Telegram fetches itself). Returns the fallback-error without the
/// task attached; callers wrap it with the updated task state.
async fn send_batch_via_upload(
    bot: &Bot,
    chat_id: i64,
    reply_to: i64,
    batch: &[MediaItemPayload],
    caption: Option<&str>,
) -> Result<Vec<Message>, FallbackError> {
    let mut files = Vec::new();
    let mut items = Vec::new();
    for (i, item) in batch.iter().enumerate() {
        let item_caption = if i == 0 { caption } else { None };
        // Size check before downloading/uploading: over the cap, use the
        // smaller URL instead of the file. Photos are exempt — they are
        // downloaded and processed (downscale / PNG→JPEG) before uploading.
        let too_large = match x_media::site::media_size(item_url(item)).await {
            Ok(Some(size)) => size > MAX_UPLOAD_BYTES,
            _ => false,
        };
        let too_large = too_large && !matches!(item, MediaItemPayload::Photo { .. });
        let media = if too_large {
            match item.fallback_url() {
                Some(url) => match media_from_url(item, url, item_caption, item.thumbnail_url()) {
                    Ok(media) => media,
                    Err(message) => {
                        return Err(FallbackError::Permanent { message });
                    }
                },
                None => {
                    return Err(FallbackError::Permanent {
                        message: "media too large".into(),
                    });
                }
            }
        } else {
            match download_to_temp(item).await {
                Ok(file) => {
                    // Telegram rejects photos wider+taller than 10000 px
                    // combined (PHOTO_INVALID_DIMENSIONS): downscale the
                    // downloaded file before uploading; photos that cannot be
                    // brought within the limits degrade to the smaller URL.
                    if matches!(item, MediaItemPayload::Photo { .. }) {
                        // CPU-heavy (decode/resize/encode): run off the async
                        // executor thread.
                        let prep = tokio::task::spawn_blocking(move || photo::prepare_photo(file))
                            .await
                            .map_err(|e| FallbackError::Permanent {
                                message: format!("photo worker panicked: {e}"),
                            })?
                            .map_err(|message| FallbackError::Permanent { message })?;
                        match prep {
                            PhotoPrep::Upload(upload) => {
                                let path = upload.path().to_path_buf();
                                files.push(upload);
                                media_from_file(item, path, item_caption, item.thumbnail_url())
                                    .map_err(|message| FallbackError::Permanent { message })?
                            }
                            PhotoPrep::UseFallback => match item.fallback_url() {
                                Some(url) => match media_from_url(item, url, item_caption, item.thumbnail_url()) {
                                    Ok(media) => media,
                                    Err(message) => {
                                        return Err(FallbackError::Permanent { message });
                                    }
                                },
                                None => {
                                    return Err(FallbackError::Permanent {
                                        message:
                                            "photo dimensions exceed Telegram limits and no smaller variant is available"
                                                .into(),
                                    });
                                }
                            },
                        }
                    } else {
                        let path = file.path().to_path_buf();
                        files.push(file);
                        media_from_file(item, path, item_caption, item.thumbnail_url())
                            .map_err(|message| FallbackError::Permanent { message })?
                    }
                }
                Err(FallbackError::MediaTooLarge) => match item.fallback_url() {
                    Some(url) => match media_from_url(item, url, item_caption, item.thumbnail_url()) {
                        Ok(media) => media,
                        Err(message) => {
                            return Err(FallbackError::Permanent { message });
                        }
                    },
                    None => {
                        return Err(FallbackError::Permanent {
                            message: "media too large".into(),
                        });
                    }
                },
                Err(e) => return Err(e),
            }
        };
        items.push(media);
    }
    let result = bot
        .send_media_group(ChatId(chat_id), items)
        .reply_parameters(
            ReplyParameters::new(MessageId(reply_to as i32)).allow_sending_without_reply(),
        )
        .await;
    match result {
        Ok(messages) => Ok(messages),
        Err(e) => Err(match classify_request_error(&e) {
            Classification::Retryable { delay_seconds } => {
                FallbackError::Retryable { delay_seconds }
            }
            Classification::Permanent { message } => FallbackError::Permanent { message },
            Classification::MediaFetchFailure => FallbackError::Permanent {
                message: "upload failed".into(),
            },
        }),
    }
}

fn updated_sequence_task(task: &Task, batch_index: usize, sent_message_ids: Vec<i64>) -> Task {
    match task {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id,
            caption,
            media_batches,
            batch_index: _,
            sent_message_ids: _,
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
            cache_data,
        } => Task::SendMediaSequence {
            chat_id: *chat_id,
            reply_to_message_id: *reply_to_message_id,
            caption: caption.clone(),
            media_batches: media_batches.clone(),
            batch_index,
            sent_message_ids,
            source_url: source_url.clone(),
            edit_before_forward: *edit_before_forward,
            forward_channel_id: *forward_channel_id,
            notify_chat_id: *notify_chat_id,
            notify_message_id: *notify_message_id,
            cache_data: cache_data.clone(),
        },
        _ => unreachable!("updated_sequence_task requires a SendMediaSequence task"),
    }
}

/// Sends the media batches starting at `task.batch_index`, extending
/// `sent_message_ids`. Returns all sent message ids on full success; on
/// failure returns a [`SendError`] whose task carries the resumed state.
pub async fn send_media_sequence(bot: &Bot, task: &Task) -> Result<Vec<i64>, SendError> {
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
    let mut sent = sent_message_ids.clone();
    // File ids accumulated across batches for the link cache. Only a fresh
    // (non-resumed) full send populates the cache.
    let mut cached_media: Vec<CachedMedia> = Vec::new();
    let fresh_send = *batch_index == 0 && sent.is_empty();
    for idx in *batch_index..media_batches.len() {
        let batch = &media_batches[idx];
        let caption = if idx == 0 {
            Some(caption.as_str())
        } else {
            None
        };
        let items = match build_media_group(batch, caption) {
            Ok(items) => items,
            Err(message) => {
                return Err(SendError::Permanent {
                    message,
                    task: updated_sequence_task(task, idx, sent),
                });
            }
        };
        match bot
            .send_media_group(ChatId(chat_id), items)
            .reply_parameters(
                ReplyParameters::new(MessageId(reply_to as i32)).allow_sending_without_reply(),
            )
            .await
        {
            Ok(messages) => {
                log::info!(
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
                    batch.first().map(item_url).unwrap_or("?")
                );
                match send_batch_via_upload(bot, chat_id, reply_to, batch, caption).await {
                    Ok(messages) => {
                        collect_file_ids(&messages, batch, &mut cached_media);
                        sent.extend(messages.into_iter().map(|m| m.id.0 as i64));
                    }
                    Err(FallbackError::Retryable { delay_seconds }) => {
                        return Err(SendError::Retryable {
                            delay_seconds,
                            task: updated_sequence_task(task, idx, sent),
                        });
                    }
                    Err(FallbackError::Permanent { message }) => {
                        return Err(SendError::Permanent {
                            message,
                            task: updated_sequence_task(task, idx, sent),
                        });
                    }
                    Err(FallbackError::MediaTooLarge) => unreachable!("handled inside upload"),
                }
            }
            Err(e) => {
                return Err(classify_to_send_error(
                    &e,
                    updated_sequence_task(task, idx, sent),
                ));
            }
        }
    }
    if fresh_send {
        cache_sent_task(task, cached_media).await;
    }
    Ok(sent)
}

async fn send_animation_inner(
    bot: &Bot,
    chat_id: i64,
    reply_to: i64,
    caption: &str,
    spoiler: bool,
    file: InputFile,
) -> Result<Message, RequestError> {
    let mut request = bot
        .send_animation(ChatId(chat_id), file)
        .caption(caption)
        .parse_mode(ParseMode::Html)
        .reply_parameters(
            ReplyParameters::new(MessageId(reply_to as i32)).allow_sending_without_reply(),
        );
    if spoiler {
        request = request.has_spoiler(true);
    }
    request.await
}

/// Sends a lone animation (gif), URL first with the download fallback.
pub async fn send_animation(bot: &Bot, task: &Task) -> Result<Vec<i64>, SendError> {
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
                task: task.clone(),
            });
        }
    };
    match send_animation_inner(bot, chat_id, reply_to, caption, has_spoiler, url_file).await {
        Ok(message) => {
            let id = message.id.0 as i64;
            cache_animation_send(task, &message).await;
            Ok(vec![id])
        }
        Err(RequestError::Api(api)) if is_media_fetch_failure(&api) || is_size_error(&api) => {
            log::info!(
                "Telegram could not fetch animation URL, downloading and reuploading: {}",
                media_url
            );
            match download_to_temp(animation).await {
                Ok(file) => {
                    let path = file.path().to_path_buf();
                    match send_animation_inner(
                        bot,
                        chat_id,
                        reply_to,
                        caption,
                        has_spoiler,
                        InputFile::file(path),
                    )
                    .await
                    {
                        Ok(message) => {
                            let id = message.id.0 as i64;
                            cache_animation_send(task, &message).await;
                            Ok(vec![id])
                        }
                        Err(e) => Err(classify_to_send_error(&e, task.clone())),
                    }
                }
                // Over the upload cap: fall back to the smaller URL.
                Err(FallbackError::MediaTooLarge) => match animation.fallback_url() {
                    Some(url) => match input_file_for(url) {
                        Ok(file) => {
                            match send_animation_inner(
                                bot,
                                chat_id,
                                reply_to,
                                caption,
                                has_spoiler,
                                file,
                            )
                            .await
                            {
                                Ok(message) => {
                                    let id = message.id.0 as i64;
                                    cache_animation_send(task, &message).await;
                                    Ok(vec![id])
                                }
                                Err(e) => Err(classify_to_send_error(&e, task.clone())),
                            }
                        }
                        Err(message) => Err(SendError::Permanent {
                            message,
                            task: task.clone(),
                        }),
                    },
                    None => Err(SendError::Permanent {
                        message: "media too large".into(),
                        task: task.clone(),
                    }),
                },
                Err(FallbackError::Retryable { delay_seconds }) => Err(SendError::Retryable {
                    delay_seconds,
                    task: task.clone(),
                }),
                Err(FallbackError::Permanent { message }) => Err(SendError::Permanent {
                    message,
                    task: task.clone(),
                }),
            }
        }
        Err(e) => Err(classify_to_send_error(&e, task.clone())),
    }
}

/// Copies already-sent messages to the forward channel. No download fallback:
/// the files are already on Telegram's servers.
pub async fn forward_messages(bot: &Bot, task: &Task) -> Result<(), SendError> {
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
    match bot
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
        Err(e) => Err(classify_to_send_error(&e, task.clone())),
    }
}

/// One button per template name (column layout), then the confirm button.
pub fn build_edit_markup(templates: &HashMap<String, String>) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    for name in templates.keys() {
        rows.push(vec![InlineKeyboardButton::callback(
            name.clone(),
            format!("template|{name}"),
        )]);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "↩️ Confirm",
        "forward",
    )]);
    InlineKeyboardMarkup::new(rows)
}

/// Notifies a chat about a dead-lettered task (skips when `notify_chat_id` is
/// absent).
pub async fn notify_failure(
    bot: &Bot,
    chat_id: Option<i64>,
    message_id: Option<i64>,
    message: &str,
) {
    let Some(chat_id) = chat_id else { return };
    let mut request = bot.send_message(ChatId(chat_id), message);
    if let Some(message_id) = message_id {
        request = request.reply_parameters(
            ReplyParameters::new(MessageId(message_id as i32)).allow_sending_without_reply(),
        );
    }
    if let Err(e) = request.await {
        log::error!("failed to notify about failed task: {e}");
    }
}

/// After a successful send: either open the edit-before-forward prompt or
/// forward to the configured channel (with retry/queue handling).
pub async fn post_send_actions(bot: &Bot, task: &Task, message_ids: Vec<i64>) {
    let (
        chat_id,
        reply_to,
        source_url,
        edit_before_forward,
        forward_channel_id,
        notify_chat_id,
        notify_message_id,
    ) = match task {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id,
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
            ..
        }
        | Task::SendAnimation {
            chat_id,
            reply_to_message_id,
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
            ..
        } => (
            *chat_id,
            *reply_to_message_id,
            source_url.clone(),
            *edit_before_forward,
            *forward_channel_id,
            *notify_chat_id,
            *notify_message_id,
        ),
        Task::ForwardMessages { .. } => return,
    };

    if edit_before_forward {
        let keyboard = build_edit_markup(&CHAT_STORE.get(chat_id).await.template);
        match bot
            .send_message(ChatId(chat_id), "Reply to edit message.")
            .reply_markup(keyboard)
            .reply_parameters(
                ReplyParameters::new(MessageId(reply_to as i32)).allow_sending_without_reply(),
            )
            .await
        {
            Ok(prompt) => {
                log::info!(
                    "edit-before-forward prompt {} opened for {} message(s)",
                    prompt.id.0,
                    message_ids.len()
                );
                let prompt_id = prompt.id.0 as i64;
                let source_url = source_url.clone();
                CHAT_STORE
                    .update(chat_id, move |data| {
                        data.edit_message.insert(
                            prompt_id,
                            EditMessage {
                                url: source_url,
                                chat_id,
                                forward_message_ids: message_ids,
                                template: String::new(),
                                created_at: unix_now(),
                            },
                        );
                    })
                    .await;
            }
            Err(e) => log::error!("failed to send edit prompt: {e}"),
        }
        return;
    }

    if let Some(channel_id) = forward_channel_id {
        log::info!(
            "forwarding {} message(s) to channel {channel_id}",
            message_ids.len()
        );
        let forward_task = Task::ForwardMessages {
            from_chat_id: chat_id,
            to_chat_id: channel_id,
            message_ids,
            notify_chat_id,
            notify_message_id,
        };
        match forward_messages(bot, &forward_task).await {
            Ok(()) => {}
            Err(SendError::Retryable {
                delay_seconds,
                task,
            }) => {
                let payload = serde_json::to_value(task).expect("task serializes");
                let run_after = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0)
                    + delay_seconds;
                if let Err(e) = TASK_QUEUE.enqueue(payload, run_after).await {
                    log::error!("failed to enqueue forward retry: {e}");
                }
            }
            Err(SendError::Permanent { message, .. }) => {
                notify_failure(
                    bot,
                    notify_chat_id,
                    notify_message_id,
                    &format!("Task failed after retries: {message}"),
                )
                .await;
            }
        }
    }
}

/// Queue entry point: parses the stored task and dispatches.
pub async fn handle_task(payload: serde_json::Value) -> Result<(), QueueError> {
    let task: Task = match serde_json::from_value(payload.clone()) {
        Ok(task) => task,
        Err(e) => {
            return Err(QueueError::Permanent {
                message: format!("invalid task payload: {e}"),
                payload,
            });
        }
    };
    let bot = BOT.clone();
    // A resumed multi-batch send already ran post_send_actions (edit prompt /
    // forward) when it first started; running them again on the resume would
    // open a duplicate edit prompt and double-forward. SendAnimation is
    // atomic (always a fresh run), so only SendMediaSequence can resume.
    let resumed = matches!(
        &task,
        Task::SendMediaSequence {
            batch_index,
            sent_message_ids,
            ..
        } if *batch_index > 0 || !sent_message_ids.is_empty()
    );
    match task {
        Task::SendMediaSequence { .. } | Task::SendAnimation { .. } => {
            let message_ids = match send_media_or_animation(&bot, &task).await {
                Ok(ids) => ids,
                Err(SendError::Retryable {
                    delay_seconds,
                    task,
                }) => {
                    return Err(QueueError::Retryable {
                        delay_seconds,
                        payload: serde_json::to_value(task).expect("task serializes"),
                    });
                }
                Err(SendError::Permanent { message, task }) => {
                    invalidate_cache(&task).await;
                    return Err(QueueError::Permanent {
                        message,
                        payload: serde_json::to_value(task).expect("task serializes"),
                    });
                }
            };
            if !resumed {
                post_send_actions(&bot, &task, message_ids).await;
            }
            Ok(())
        }
        Task::ForwardMessages { .. } => match forward_messages(&bot, &task).await {
            Ok(()) => Ok(()),
            Err(SendError::Retryable {
                delay_seconds,
                task,
            }) => Err(QueueError::Retryable {
                delay_seconds,
                payload: serde_json::to_value(task).expect("task serializes"),
            }),
            Err(SendError::Permanent { message, task }) => Err(QueueError::Permanent {
                message,
                payload: serde_json::to_value(task).expect("task serializes"),
            }),
        },
    }
}

async fn send_media_or_animation(bot: &Bot, task: &Task) -> Result<Vec<i64>, SendError> {
    match task {
        Task::SendMediaSequence { .. } => send_media_sequence(bot, task).await,
        Task::SendAnimation { .. } => send_animation(bot, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    }
}

/// Dead-letter callback wired to the queue in main: notifies the task's chat.
pub async fn dead_letter_notify(payload: serde_json::Value, message: String) {
    let notify_chat_id = payload.get("notify_chat_id").and_then(|v| v.as_i64());
    let notify_message_id = payload.get("notify_message_id").and_then(|v| v.as_i64());
    if notify_chat_id.is_some() {
        let bot = BOT.clone();
        notify_failure(
            &bot,
            notify_chat_id,
            notify_message_id,
            &format!("Task failed after retries: {message}"),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_photo_boundary() {
        // The empirical Telegram limit: sum 10000 passes, 10001 fails.
        assert!(crate::photo::PHOTO_MAX_DIMENSION_SUM == 10000);
        assert!(6100 + 3900 <= crate::photo::PHOTO_MAX_DIMENSION_SUM);
        assert!(6300 + 3730 > crate::photo::PHOTO_MAX_DIMENSION_SUM);
    }

    #[test]
    fn chunk_media_items_sizes() {
        assert_eq!(chunk_media_items::<i32>(vec![]), Vec::<Vec<i32>>::new());
        assert_eq!(chunk_media_items((0..9).collect()).len(), 1);
        assert_eq!(chunk_media_items((0..10).collect()).len(), 2);
        assert_eq!(chunk_media_items((0..25).collect()).len(), 3);
        assert_eq!(chunk_media_items((0..25).collect())[2].len(), 7);
        assert!(
            chunk_media_items((0..25).collect())
                .iter()
                .all(|c| c.len() <= 9)
        );
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
    fn is_media_fetch_failure_matches_markers() {
        for description in [
            "Bad Request: WEBPAGE_MEDIA_EMPTY",
            "Bad Request: media_empty",
            "Bad Request: EMPTY_WEB_MEDIA",
            "Bad Request: webpage_curl_failed",
            "Bad Request: request timeout",
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
}
