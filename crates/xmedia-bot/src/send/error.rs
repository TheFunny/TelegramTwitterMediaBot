//! The Telegram error policy: which failures the send paths retry, which are
//! permanent, and which the download-and-reupload fallback owns. A status a
//! *site* answers with is classified in `x_media::site`; this is the Bot API's
//! side of the same question.

use super::upload::FallbackError;
use super::{Task, retry_delay_seconds};
use teloxide::{ApiError, RequestError};

/// Telegram's servers failed to fetch a media URL (hotlink protection etc.):
/// these errors are handled by the download-and-reupload fallback, NOT by a
/// queue retry (resending the URL cannot succeed).
pub fn is_media_fetch_failure(e: &ApiError) -> bool {
    const MARKERS: [&str; 7] = [
        "webpage_media_empty",
        "media_empty",
        "empty_web_media",
        "webpage_curl_failed",
        "timeout",
        // Oversized photos (width + height > 10000 px) are rejected on URL
        // sends too; route them to the download-and-resize fallback.
        "photo_invalid_dimensions",
        // Telegram refused to fetch the URL it was handed. Single-media URL
        // sends answer with this one (the media-group verbs use the
        // `webpage_*`/`media_empty` markers above), and it is exactly the
        // case the download-and-reupload fallback exists for.
        "failed to get http url content",
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
        // A 5xx from the API — or from a proxy in front of it — is transient.
        // teloxide only sleeps 10s on a server error and then parses whatever
        // body came back, so by the time we see the error the HTTP status is
        // gone: a JSON 5xx body arrives as an unknown description, an HTML
        // error page as `InvalidJson`. Both used to be Permanent, which
        // dead-lettered a post over a Telegram-side blip.
        RequestError::Api(api) if is_server_error_text(&api.to_string()) => {
            Classification::Retryable {
                delay_seconds: retry_delay_seconds(0),
            }
        }
        RequestError::Api(api) if is_media_fetch_failure(api) => Classification::MediaFetchFailure,
        RequestError::Api(api) => Classification::Permanent {
            message: api.to_string(),
        },
        // An unparsable body can only come from something that is not the Bot
        // API (which always answers JSON): a 5xx/error page from an
        // intermediary, cut off mid-response. A JSON body that merely does not
        // match the expected type cannot be fixed by retrying, so that case
        // stays permanent.
        RequestError::InvalidJson { raw, .. } if !raw.trim_start().starts_with('{') => {
            Classification::Retryable {
                delay_seconds: retry_delay_seconds(0),
            }
        }
        RequestError::MigrateToChatId(_)
        | RequestError::InvalidJson { .. }
        | RequestError::Io(_) => Classification::Permanent {
            message: e.to_string(),
        },
    }
}

/// Descriptions a 5xx carries when its body *is* JSON (teloxide keeps only the
/// description text, never the status code). Matched like the media-fetch
/// markers below; anything unmatched stays permanent, so a new permanent API
/// error is not retried just because it is unfamiliar.
fn is_server_error_text(description: &str) -> bool {
    const MARKERS: [&str; 4] = [
        "server error",
        "bad gateway",
        "gateway timeout",
        "service unavailable",
    ];
    let description = description.to_lowercase();
    MARKERS.iter().any(|marker| description.contains(marker))
}

/// Task boxed to keep the error size within `result_large_err` limits.
#[derive(Debug)]
pub enum SendError {
    Retryable { delay_seconds: f64, task: Box<Task> },
    Permanent { message: String, task: Box<Task> },
}
pub(crate) fn classify_to_send_error(
    e: &RequestError,
    task: Task,
    fetch_failure_label: &str,
) -> SendError {
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
    pub(super) fn from_fallback(f: FallbackError, task: Task) -> SendError {
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
