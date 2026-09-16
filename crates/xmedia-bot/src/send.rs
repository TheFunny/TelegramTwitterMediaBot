//! Typed task payloads and send/forward executors with retry classification
//! and the download-and-reupload fallback (Telegram's own fetch of a media
//! URL is blocked by hotlink protection; the bot downloads the file itself
//! and uploads it via multipart).

use crate::db::{now_f64, unix_now};
use crate::handlers::{CHAT_STORE, LINK_CACHE, TASK_QUEUE, log_key};
use crate::link_cache::{CachedMedia, CachedMediaKind, CachedPost, LinkCache};
use crate::media_sender::MediaSender;
use crate::photo::{self, MAX_UPLOAD_BYTES, PhotoPrep};
use crate::queue::{PersistentTaskQueue, QueueError};
use crate::state::EditMessage;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaAnimation,
    InputMediaPhoto, InputMediaVideo, Message, MessageId, ParseMode,
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
        log::debug!("cached send for [key={}]", log_key(&post.url));
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
    invalidate_cache_with(&LINK_CACHE, task).await;
}

/// [`invalidate_cache`] against an injected cache (tests pass a tempdir one).
pub async fn invalidate_cache_with(cache: &LinkCache, task: &Task) {
    if task.is_cached_send()
        && let Some(url) = task.source_url()
        && let Some(key) = x_media::site::cache_key(url)
    {
        log::debug!("removing stale link cache entry for [key={}]", log_key(url));
        cache.remove(&key).await;
    }
}

/// Locally produced media files (ugoira MP4, bsky remux MP4) whose temp dirs
/// must stay alive while their task may be retried by the queue. The fetch
/// pipeline hands ownership here via [`x_media::site::Fetched::take_keep_alive`]
/// before the [`Fetched`] is dropped; a queued retry runs after that drop, so
/// without this the local file would be gone by the time the retry sends it.
/// Entries are removed when the task settles (see [`release_keep_alive`]).
pub static KEEP_ALIVE: LazyLock<parking_lot::Mutex<Vec<tempfile::TempDir>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));

/// Drops the keep-alive temp dirs holding media referenced by `task` (matched
/// by path prefix). Called once a task settles — sent or permanently failed —
/// so retry-only temp files do not leak; retryable tasks keep them alive.
pub fn release_keep_alive(task: &Task) {
    let paths = task.local_media_paths();
    if paths.is_empty() {
        return;
    }
    let mut alive = KEEP_ALIVE.lock();
    alive.retain(|dir| {
        let dir_path = dir.path();
        !paths.iter().any(|p| p.starts_with(dir_path))
    });
}

pub const MAX_MEDIA_GROUP: usize = 9;

/// Splits media into batches of at most [`MAX_MEDIA_GROUP`] items.
pub fn chunk_media_items<T: Clone>(items: Vec<T>) -> Vec<Vec<T>> {
    items
        .chunks(MAX_MEDIA_GROUP)
        .map(|chunk| chunk.to_vec())
        .collect()
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
/// Downloads one media item to a temp file (deleted on drop), returning the
/// file plus the downloaded bytes (photos keep the bytes for
/// [`photo::prepare_photo`] — re-reading the file would double the I/O).
/// Network errors are retryable; size over the upload cap and other download
/// errors are not.
async fn download_to_temp(
    item: &MediaItemPayload,
) -> Result<(NamedTempFile, bytes::Bytes), FallbackError> {
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
    Ok((file, bytes))
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

/// One item prepared for the upload fallback: the ready-to-send media plus
/// the temp file that must stay on disk until the group request completes.
struct PreparedItem {
    /// Original position in the batch (concurrent prep completes out of order).
    index: usize,
    media: InputMedia,
    keep_alive: Option<NamedTempFile>,
}

/// Downloads / processes one media item for the upload fallback (see
/// [`send_batch_via_upload`]). Local files are uploaded directly; oversized
/// items fall back to their smaller URL; photos are downscaled/transcoded.
async fn prepare_upload_item(
    item: MediaItemPayload,
    index: usize,
    caption: Option<&str>,
) -> Result<PreparedItem, FallbackError> {
    // Locally produced files (ugoira / bsky remux MP4): nothing to download
    // or shrink — upload the file directly. The send is a multipart upload,
    // so the only remaining failure is an upload-cap error, which is
    // permanent (a video cannot be re-encoded here).
    let media_url = item_url(&item);
    if !media_url.starts_with("http://") && !media_url.starts_with("https://") {
        let media = media_from_file(
            &item,
            std::path::PathBuf::from(media_url),
            caption,
            item.thumbnail_url(),
        )
        .map_err(|message| FallbackError::Permanent { message })?;
        return Ok(PreparedItem {
            index,
            media,
            keep_alive: None,
        });
    }
    // Size check before downloading/uploading: over the cap, use the
    // smaller URL instead of the file. Photos are exempt — they are
    // downloaded and processed (downscale / PNG→JPEG) before uploading.
    let too_large = match x_media::site::media_size(media_url).await {
        Ok(Some(size)) => size > MAX_UPLOAD_BYTES,
        _ => false,
    };
    let too_large = too_large && !matches!(item, MediaItemPayload::Photo { .. });
    if too_large {
        let url = item
            .fallback_url()
            .ok_or_else(|| FallbackError::Permanent {
                message: "media too large".into(),
            })?;
        let media = media_from_url(&item, url, caption, item.thumbnail_url())
            .map_err(|message| FallbackError::Permanent { message })?;
        return Ok(PreparedItem {
            index,
            media,
            keep_alive: None,
        });
    }
    match download_to_temp(&item).await {
        Ok((file, bytes)) => {
            if matches!(item, MediaItemPayload::Photo { .. }) {
                // Telegram rejects photos wider+taller than 10000 px combined
                // (PHOTO_INVALID_DIMENSIONS): downscale the downloaded file
                // before uploading; photos that cannot be brought within the
                // limits degrade to the smaller URL. CPU-heavy work runs off
                // the async executor thread.
                let prep = tokio::task::spawn_blocking(move || photo::prepare_photo(file, &bytes))
                    .await
                    .map_err(|e| FallbackError::Permanent {
                        message: format!("photo worker panicked: {e}"),
                    })?
                    .map_err(|message| FallbackError::Permanent { message })?;
                match prep {
                    PhotoPrep::Upload(upload) => {
                        let path = upload.path().to_path_buf();
                        let media = media_from_file(&item, path, caption, item.thumbnail_url())
                            .map_err(|message| FallbackError::Permanent { message })?;
                        Ok(PreparedItem {
                            index,
                            media,
                            keep_alive: Some(upload),
                        })
                    }
                    PhotoPrep::UseFallback => {
                        let url = item.fallback_url().ok_or_else(|| FallbackError::Permanent {
                            message: "photo dimensions exceed Telegram limits and no smaller variant is available"
                                .into(),
                        })?;
                        let media = media_from_url(&item, url, caption, item.thumbnail_url())
                            .map_err(|message| FallbackError::Permanent { message })?;
                        Ok(PreparedItem {
                            index,
                            media,
                            keep_alive: None,
                        })
                    }
                }
            } else {
                let path = file.path().to_path_buf();
                let media = media_from_file(&item, path, caption, item.thumbnail_url())
                    .map_err(|message| FallbackError::Permanent { message })?;
                Ok(PreparedItem {
                    index,
                    media,
                    keep_alive: Some(file),
                })
            }
        }
        Err(FallbackError::MediaTooLarge) => {
            let url = item
                .fallback_url()
                .ok_or_else(|| FallbackError::Permanent {
                    message: "media too large".into(),
                })?;
            let media = media_from_url(&item, url, caption, item.thumbnail_url())
                .map_err(|message| FallbackError::Permanent { message })?;
            Ok(PreparedItem {
                index,
                media,
                keep_alive: None,
            })
        }
        Err(e) => Err(e),
    }
}

/// Download-and-reupload fallback for one media batch. Files over the upload
/// cap are not downloaded/uploaded; the item falls back to its smaller URL
/// (which Telegram fetches itself). Items are prepared concurrently (bounded)
/// because the downloads are network-bound; the batch is then uploaded in its
/// original order. Returns the fallback-error without the task attached;
/// callers wrap it with the updated task state.
async fn send_batch_via_upload(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: i64,
    batch: &[MediaItemPayload],
    caption: Option<&str>,
    task: Task,
) -> Result<Vec<Message>, SendError> {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
    let mut set = tokio::task::JoinSet::new();
    for (i, item) in batch.iter().enumerate() {
        let item_caption = if i == 0 {
            caption.map(str::to_string)
        } else {
            None
        };
        let item = item.clone();
        let sem = std::sync::Arc::clone(&sem);
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("upload semaphore closed");
            prepare_upload_item(item, i, item_caption.as_deref()).await
        });
    }
    let mut prepared: Vec<Option<InputMedia>> = (0..batch.len()).map(|_| None).collect();
    let mut keep_alive: Vec<NamedTempFile> = Vec::new();
    while let Some(joined) = set.join_next().await {
        let item = match joined {
            Ok(Ok(item)) => item,
            // Dropping the JoinSet aborts the remaining prep tasks; their
            // temp files are cleaned up on drop (short-circuit like before).
            Ok(Err(e)) => return Err(SendError::from_fallback(e, task.clone())),
            Err(e) => {
                return Err(SendError::Permanent {
                    message: format!("upload worker panicked: {e}"),
                    task: Box::new(task),
                });
            }
        };
        let PreparedItem {
            index,
            media,
            keep_alive: file_opt,
        } = item;
        if let Some(file) = file_opt {
            keep_alive.push(file);
        }
        prepared[index] = Some(media);
    }
    let items: Vec<InputMedia> = prepared
        .into_iter()
        .map(|m| m.expect("every upload item was prepared"))
        .collect();
    // `keep_alive` holds the temp files until the group request completes.
    let result = sender
        .send_media_group(ChatId(chat_id), MessageId(reply_to as i32), items)
        .await;
    drop(keep_alive);
    match result {
        Ok(messages) => Ok(messages),
        Err(e) => Err(classify_to_send_error(&e, task, "upload failed")),
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

/// Sends the media batches starting at `task.batch_index`, extending
/// `sent_message_ids`. Returns all sent message ids on full success; on
/// failure returns a [`SendError`] whose task carries the resumed state.
pub async fn send_media_sequence(
    sender: &dyn MediaSender,
    task: &Task,
) -> Result<Vec<i64>, SendError> {
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
                    task: Box::new(updated_sequence_task(task, idx, sent)),
                });
            }
        };
        match sender
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
                    sender,
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
        cache_sent_task(task, cached_media).await;
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
pub async fn send_animation(sender: &dyn MediaSender, task: &Task) -> Result<Vec<i64>, SendError> {
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
                task: Box::new(task.clone()),
            });
        }
    };
    match send_animation_inner(sender, chat_id, reply_to, caption, has_spoiler, url_file).await {
        Ok(message) => {
            let id = message.id.0 as i64;
            cache_animation_send(task, &message).await;
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
                        sender,
                        chat_id,
                        reply_to,
                        caption,
                        has_spoiler,
                        animation.media,
                    )
                    .await
                    {
                        Ok(message) => {
                            let id = message.id.0 as i64;
                            cache_animation_send(task, &message).await;
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
pub async fn forward_messages(sender: &dyn MediaSender, task: &Task) -> Result<(), SendError> {
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
    match sender
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
    sender: &dyn MediaSender,
    chat_id: Option<i64>,
    message_id: Option<i64>,
    message: &str,
) {
    let Some(chat_id) = chat_id else { return };
    let reply_to = message_id.map(|id| MessageId(id as i32));
    if let Err(e) = sender
        .send_message(ChatId(chat_id), message.to_string(), reply_to, None)
        .await
    {
        log::error!("failed to notify about failed task: {e}");
    }
}

/// After a successful send: either open the edit-before-forward prompt or
/// forward to the configured channel (with retry/queue handling).
pub async fn post_send_actions(sender: &dyn MediaSender, task: &Task, message_ids: Vec<i64>) {
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
        let prompt = sender
            .send_message(
                ChatId(chat_id),
                "Reply to edit message.".to_string(),
                Some(MessageId(reply_to as i32)),
                Some(keyboard),
            )
            .await;
        match prompt {
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
        match forward_messages(sender, &forward_task).await {
            Ok(()) => {}
            Err(SendError::Retryable {
                delay_seconds,
                task,
            }) => {
                enqueue_retry(&TASK_QUEUE, *task, delay_seconds).await;
            }
            Err(SendError::Permanent { message, .. }) => {
                notify_failure(
                    sender,
                    notify_chat_id,
                    notify_message_id,
                    &format!("Task failed after retries: {message}"),
                )
                .await;
            }
        }
    }
}

/// Enqueues a task for a later attempt (retry / forward resume). When the
/// enqueue itself fails the task can never be sent again, so its keep-alive
/// temp media is released instead of leaking until process exit.
pub async fn enqueue_retry(queue: &PersistentTaskQueue, task: Task, delay_seconds: f64) {
    let payload = serde_json::to_value(&task).expect("task serializes");
    let run_after = now_f64() + delay_seconds;
    if let Err(e) = queue.enqueue(payload, run_after).await {
        log::error!("failed to enqueue retry: {e}");
        release_keep_alive(&task);
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
                    // The task settles here: drop any keep-alive temp media.
                    release_keep_alive(&task);
                    return Err(QueueError::Permanent {
                        message,
                        payload: serde_json::to_value(task).expect("task serializes"),
                    });
                }
            };
            // A task only reaches the queue after a failed send, so this
            // successful run is the first time post_send_actions can fire —
            // the fresh attempt failed before it ever got here. Run it
            // unconditionally: `post_send_actions` executes once, after the
            // whole sequence (every batch) completed, so the channel forward
            // and the edit-before-forward prompt must not be lost just
            // because the send needed a retry.
            post_send_actions(&bot, &task, message_ids).await;
            release_keep_alive(&task);
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
            Err(SendError::Permanent { message, task }) => {
                release_keep_alive(&task);
                Err(QueueError::Permanent {
                    message,
                    payload: serde_json::to_value(task).expect("task serializes"),
                })
            }
        },
    }
}

async fn send_media_or_animation(
    sender: &dyn MediaSender,
    task: &Task,
) -> Result<Vec<i64>, SendError> {
    match task {
        Task::SendMediaSequence { .. } => send_media_sequence(sender, task).await,
        Task::SendAnimation { .. } => send_animation(sender, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    }
}

/// Dead-letter callback wired to the queue in main: notifies the task's chat.
pub async fn dead_letter_notify(payload: serde_json::Value, message: String) {
    // A dead-lettered task never runs again. The queue dead-letters retry
    // exhaustion itself (the handler is not called again), so this is the
    // only place that sees the final payload — release the keep-alive temp
    // media the fetch pipeline handed over, or it lives until process exit.
    if let Ok(task) = serde_json::from_value::<Task>(payload.clone()) {
        release_keep_alive(&task);
    }
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
        // Const-block asserts so clippy's assertions_on_constants stays quiet.
        const { assert!(crate::photo::PHOTO_MAX_DIMENSION_SUM == 10000) };
        const { assert!(6100 + 3900 <= crate::photo::PHOTO_MAX_DIMENSION_SUM) };
        const { assert!(6300 + 3730 > crate::photo::PHOTO_MAX_DIMENSION_SUM) };
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
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
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
            cache_data: None,
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
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&sender, &task).await;
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
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&sender, &task).await;
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
        let result = send_animation(&sender, &task).await;
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
        let task = sequence_task(file.to_str().unwrap());
        let result = send_media_sequence(&sender, &task).await;
        assert!(result.is_ok(), "got {result:?}");

        let sender = MockSender::scripted(vec![Outcome::CopyOk], media_fetch_error);
        let task = Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            notify_chat_id: None,
            notify_message_id: None,
        };
        assert!(forward_messages(&sender, &task).await.is_ok());
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
        match forward_messages(&sender, &task).await {
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
        assert!(matches!(
            forward_messages(&sender, &task).await,
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

        let payload = serde_json::to_value(&task).unwrap();
        dead_letter_notify(payload, "task failed after 2 retries".into()).await;

        assert!(
            !KEEP_ALIVE.lock().iter().any(|dir| dir.path() == dir_path),
            "dead-lettered task kept its temp media alive"
        );
    }
}
