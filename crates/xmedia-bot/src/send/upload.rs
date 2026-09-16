//! Download-and-reupload fallback: when Telegram cannot fetch a media URL
//! itself (hotlink protection), the bot downloads the file, shrinks photos
//! that exceed Telegram's limits and uploads the batch via multipart.

use super::input_media::{animation_media, input_file_for, item_url, photo_media, video_media};
use super::{MediaItemPayload, SendError, Task, classify_to_send_error, retry_delay_seconds};
use crate::media_sender::MediaSender;
use crate::photo::{self, MAX_UPLOAD_BYTES, PhotoPrep};
use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, InputMedia, MessageId};
use tempfile::NamedTempFile;
use x_media::site::FetchError;

/// Infers a file extension from magic bytes so Telegram detects the mime type
/// on multipart uploads.
pub(super) fn sniff_ext(bytes: &[u8]) -> &'static str {
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

pub(super) enum FallbackError {
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
pub(super) struct PreparedItem {
    /// Original position in the batch (concurrent prep completes out of order).
    pub(super) index: usize,
    pub(super) media: InputMedia,
    pub(super) keep_alive: Option<NamedTempFile>,
}

/// Downloads / processes one media item for the upload fallback (see
/// [`send_batch_via_upload`]). Local files are uploaded directly; oversized
/// items fall back to their smaller URL; photos are downscaled/transcoded.
pub(super) async fn prepare_upload_item(
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
pub(super) async fn send_batch_via_upload(
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
