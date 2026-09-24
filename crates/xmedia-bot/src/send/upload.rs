//! Download-and-reupload fallback: when Telegram cannot fetch a media URL
//! itself (hotlink protection), the bot downloads the file, shrinks photos
//! that exceed Telegram's limits and uploads the batch via multipart.

use super::input_media::{input_file_for, item_url, media_from};
use super::{
    MediaItemPayload, MediaRef, SendError, Task, classify_to_send_error, retry_delay_seconds,
};
use crate::media_sender::MediaSender;
use crate::photo::{self, PhotoPrep};
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, InputMedia, MessageId};
use tempfile::NamedTempFile;
use x_media::site::FetchError;

/// How many fallback items may be downloaded and processed at once, across the
/// whole process. A per-batch bound is not a memory bound: `URL_WORKERS` (8)
/// and the queue's workers (4) can each be inside a batch, so a per-batch three
/// allowed two dozen downloads in flight, each buffering a whole photo
/// (up to [`photo::MAX_PHOTO_DOWNLOAD_BYTES`]) before it is processed. This is
/// the only admission control on the media path; the send itself is paced by
/// the rate limiter.
const PREP_CONCURRENCY: usize = 6;
static PREP_SLOTS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(PREP_CONCURRENCY));

/// Telegram's multipart upload limit for everything that is not a photo:
/// its own docs on `sendVideo`/`sendAnimation`/`sendDocument` say 50 MB
/// (`RequestEntityTooLarge` is "larger than 50 MB"), while photos are the
/// 10 MiB [`photo::MAX_UPLOAD_BYTES`] case. Using the photo cap here refused
/// to even download a 10–50 MB video that Telegram itself would have
/// accepted, and a video has no smaller variant to fall back to — so the
/// post was lost.
pub(super) const MAX_MEDIA_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;

/// Whole-transfer budget for one fallback download. The prep slot (and the
/// non-photo memory reservation) is held while this runs, and the idle window
/// alone lets a server drip one byte every 29 s forever — so this path caps
/// its own transfers well below the in-fetch default: 50 MiB in 300 s needs
/// about 1.4 Mbit/s, and a much slower link is better served by the retry
/// path toward the item's smaller fallback URL than by pinning a slot for
/// ten minutes.
/// ponytail: if slow-link reports show up, move the download out of the prep
/// slot (slot = decode/upload only) instead of raising this again.
const FALLBACK_DOWNLOAD_TOTAL: std::time::Duration = std::time::Duration::from_secs(300);

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
    media_url: &str,
) -> Result<(NamedTempFile, bytes::Bytes), FallbackError> {
    // The caller narrows the media to a source URL before calling (its entry
    // guard rejects a file id), so there is nothing to match on here.
    // Photos are downloaded even over the upload cap so `prepare_photo` can
    // downscale / transcode them, up to their own download cap; videos and
    // animations are refused as soon as the declared size crosses their own
    // (larger) upload cap. The limit is that cap, not `cap + 1`: a file of
    // exactly the cap is admitted (`len > max_bytes` is false), and one byte
    // over is not — the same boundary the size probe this replaced drew.
    let is_photo = matches!(item, MediaItemPayload::Photo { .. });
    let limit = if is_photo {
        photo::MAX_PHOTO_DOWNLOAD_BYTES
    } else {
        MAX_MEDIA_UPLOAD_BYTES
    };
    // A non-photo body is buffered whole and charges the process-wide budget
    // for as long as this function holds it (one 64 MiB unit covers the cap):
    // `PREP_SLOTS` bounds how many are in flight, this bounds what they add
    // up to. Photos charge their own download cap for the same window — their
    // real cost (header probe + decode buffer) is charged again by the
    // prepare step right after, where both are actually held together.
    let _budget = Some(
        photo::reserve_memory(if is_photo {
            photo::MAX_PHOTO_DOWNLOAD_BYTES
        } else {
            MAX_MEDIA_UPLOAD_BYTES
        })
        .await,
    );
    let bytes = match x_media::site::download_media_limited(
        media_url,
        limit,
        FALLBACK_DOWNLOAD_TOTAL,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(e) => return Err(classify_download_error(e)),
    };
    let ext = sniff_ext(&bytes);
    let mut file = tempfile::Builder::new()
        .prefix(x_media::TEMP_FILE_PREFIX)
        .suffix(&format!(".{ext}"))
        .tempfile()
        .map_err(|e| FallbackError::Permanent {
            message: format!("temp file failed: {e}"),
        })?;
    // The write runs on a blocking thread: up to 50 MiB of sync disk I/O on
    // an executor thread would stall whatever else that worker runs (six prep
    // tasks could stall six threads at once on a slow volume). A write
    // failure is resource exhaustion far more often than a broken temp
    // dir (ENOSPC / EDQUOT), and that clears on its own — worth an attempt
    // instead of dropping the post on the first try. Creating the file (above)
    // stays permanent: a temp dir that cannot be created at all is a
    // deployment fault that should fail loudly and immediately. `Retryable`
    // carries no message, so the cause is logged here.
    let (written, file, bytes) = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let written = file.as_file_mut().write_all(&bytes);
        (written, file, bytes)
    })
    .await
    .map_err(|e| FallbackError::Permanent {
        message: format!("upload write worker panicked: {e}"),
    })?;
    written.map_err(|e| {
        log::error!("temp file write failed: {e}");
        FallbackError::Retryable {
            delay_seconds: retry_delay_seconds(0),
        }
    })?;
    Ok((file, bytes))
}

/// Which failure class a media download belongs to. Transport errors and
/// server-side hiccups (429/5xx, see `download_media_limited`) are worth
/// another attempt; a 4xx means the media itself is gone or refused, and a
/// retry could only ask the same URL again.
fn classify_download_error(err: FetchError) -> FallbackError {
    match err {
        FetchError::Http(_) | FetchError::Transient(_) | FetchError::RateLimited { .. } => {
            FallbackError::Retryable {
                delay_seconds: retry_delay_seconds(0),
            }
        }
        FetchError::TooLarge => FallbackError::MediaTooLarge,
        e => FallbackError::Permanent {
            message: format!("download failed: {e}"),
        },
    }
}

/// Builds the media group item from an uploaded file.
fn media_from_file(
    item: &MediaItemPayload,
    path: std::path::PathBuf,
    caption: Option<&str>,
) -> Result<InputMedia, String> {
    media_from(item, InputFile::file(path), caption)
}

/// Builds the media group item from a (smaller) URL.
fn media_from_url(
    item: &MediaItemPayload,
    url: &str,
    caption: Option<&str>,
) -> Result<InputMedia, String> {
    media_from(item, input_file_for(url)?, caption)
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
    // A file id is already Telegram's copy of an uploaded file: there is no
    // URL to re-fetch, and without this guard `item_url` presents the id as
    // a *path*, which fails at upload time with a confusing open error
    // instead of a classification. Re-upload cannot apply to it.
    if matches!(item.media_ref(), MediaRef::FileId(_)) {
        return Err(FallbackError::Permanent {
            message: "file id reached the upload fallback".into(),
        });
    }
    // Locally produced files (ugoira / bsky remux MP4): nothing to download
    // or shrink — upload the file directly. The send is a multipart upload,
    // so the only remaining failure is an upload-cap error, which is
    // permanent (a video cannot be re-encoded here).
    let media_url = item_url(&item);
    if !media_url.starts_with("http://") && !media_url.starts_with("https://") {
        let media = media_from_file(&item, std::path::PathBuf::from(media_url), caption)
            .map_err(|message| FallbackError::Permanent { message })?;
        return Ok(PreparedItem {
            index,
            media,
            keep_alive: None,
        });
    }
    // Whether a file is over the cap is settled by the download itself:
    // `download_media_limited` reads the declared Content-Length before any
    // body byte and aborts with `FetchError::TooLarge`, which arrives here as
    // `FallbackError::MediaTooLarge` — turned into the item's smaller URL by
    // the match below. A separate size probe used to issue a second GET of the
    // same URL for an answer this path already has (and issued it for photos,
    // whose answer was discarded one line later).
    match download_to_temp(&item, media_url).await {
        Ok((file, bytes)) => {
            if matches!(item, MediaItemPayload::Photo { .. }) {
                // Telegram rejects photos wider+taller than 10000 px combined
                // (PHOTO_INVALID_DIMENSIONS): downscale the downloaded file
                // before uploading; photos that cannot be brought within the
                // limits degrade to the smaller URL. CPU-heavy work runs off
                // the async executor thread.
                //
                // The header decides what that will cost in memory, so the
                // probe travels with the downloaded bytes (both stay alive
                // through the decode) and the reservation covers their sum:
                // `PREP_SLOTS` bounds how many photos are prepared at once,
                // this bounds what they hold between them — 512 MiB, whatever
                // the batch looks like.
                let (bytes, decode) = tokio::task::spawn_blocking(move || {
                    let decode = photo::decode_budget_bytes(&bytes);
                    (bytes, decode)
                })
                .await
                .map_err(|e| FallbackError::Permanent {
                    message: format!("photo worker panicked: {e}"),
                })?;
                let _budget = photo::reserve_memory(bytes.len() as u64 + decode).await;
                let prep = tokio::task::spawn_blocking(move || photo::prepare_photo(file, &bytes))
                    .await
                    .map_err(|e| FallbackError::Permanent {
                        message: format!("photo worker panicked: {e}"),
                    })?
                    .map_err(|message| FallbackError::Permanent { message })?;
                match prep {
                    PhotoPrep::Upload(upload) => {
                        let path = upload.path().to_path_buf();
                        let media = media_from_file(&item, path, caption)
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
                        let media = media_from_url(&item, url, caption)
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
                let media = media_from_file(&item, path, caption)
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
            let media = media_from_url(&item, url, caption)
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
/// (which Telegram fetches itself). Items are prepared concurrently because the
/// downloads are network-bound, under one process-wide bound ([`PREP_SLOTS`] —
/// the URL and queue workers can each be inside a batch, so a per-batch bound
/// would multiply); the batch is then uploaded in its original order. Returns
/// the fallback-error without the task attached; callers wrap it with the
/// updated task state.
pub(super) async fn send_batch_via_upload(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: i64,
    batch: &[MediaItemPayload],
    caption: Option<&str>,
    task: Task,
) -> Result<Vec<Message>, SendError> {
    let mut set = tokio::task::JoinSet::new();
    for (i, item) in batch.iter().enumerate() {
        let item_caption = if i == 0 {
            caption.map(str::to_string)
        } else {
            None
        };
        let item = item.clone();
        set.spawn(async move {
            let _permit = PREP_SLOTS.acquire().await.expect("upload semaphore closed");
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

#[cfg(test)]
mod download_class_tests {
    use super::*;

    #[test]
    fn download_errors_split_by_whether_a_retry_can_help() {
        // Transport failure and a server-side hiccup: try again.
        assert!(matches!(
            classify_download_error(FetchError::Transient("media status 503".into())),
            FallbackError::Retryable { .. }
        ));
        // The media is gone / the host refuses us: a retry repeats the 4xx.
        assert!(matches!(
            classify_download_error(FetchError::NotFound),
            FallbackError::Permanent { .. }
        ));
        assert!(matches!(
            classify_download_error(FetchError::Blocked),
            FallbackError::Permanent { .. }
        ));
        // Over the cap: degrade to the smaller URL, never retry.
        assert!(matches!(
            classify_download_error(FetchError::TooLarge),
            FallbackError::MediaTooLarge
        ));
    }

    #[tokio::test]
    async fn a_file_id_item_is_refused_before_any_download() {
        let item = MediaItemPayload::Photo {
            media: MediaRef::FileId("AgACAgIAAx".into()),
            has_spoiler: false,
            fallback_url: None,
        };
        match prepare_upload_item(item, 0, None).await {
            Err(FallbackError::Permanent { .. }) => {}
            Err(_) => panic!("expected a permanent classification, got a different error"),
            Ok(_) => panic!("a file id must be refused, not prepared"),
        }
    }
}
