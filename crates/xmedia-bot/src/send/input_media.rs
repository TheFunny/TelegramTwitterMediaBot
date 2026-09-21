//! Payload → Telegram input types: `InputFile` selection (cached file id /
//! URL / local path), the per-kind `InputMedia` builders and the media-group
//! assembly with its caption rule.

use super::{MediaItemPayload, MediaRef};
use teloxide::types::{
    InputFile, InputMedia, InputMediaAnimation, InputMediaPhoto, InputMediaVideo, ParseMode,
};

/// The item's media string, whether it is a URL/path or a file id — callers
/// that need the distinction match on [`MediaRef`] themselves.
pub(super) fn item_url(item: &MediaItemPayload) -> &str {
    match item.media_ref() {
        MediaRef::Source(media) | MediaRef::FileId(media) => media,
    }
}

/// Remote http(s) URLs are handed to Telegram to fetch; everything else
/// (e.g. a locally encoded ugoira MP4) is uploaded directly.
pub(super) fn input_file_for(media: &str) -> Result<InputFile, String> {
    if media.starts_with("http://") || media.starts_with("https://") {
        let url = url::Url::parse(media).map_err(|e| format!("invalid media URL: {e}"))?;
        Ok(InputFile::url(url))
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
    pub(super) fn input_file(&self) -> Result<InputFile, String> {
        match self.media_ref() {
            MediaRef::FileId(id) => Ok(InputFile::file_id(id.clone().into())),
            MediaRef::Source(media) => input_file_for(media),
        }
    }
}

pub(super) fn photo_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut photo = InputMediaPhoto::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        photo = photo.caption(caption);
    }
    if spoiler {
        photo = photo.spoiler();
    }
    InputMedia::Photo(photo)
}

pub(super) fn video_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut video = InputMediaVideo::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        video = video.caption(caption);
    }
    if spoiler {
        video = video.spoiler();
    }
    InputMedia::Video(video)
}

pub(super) fn animation_media(file: InputFile, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let mut animation = InputMediaAnimation::new(file).parse_mode(ParseMode::Html);
    if let Some(caption) = caption {
        animation = animation.caption(caption);
    }
    if spoiler {
        animation = animation.spoiler();
    }
    InputMedia::Animation(animation)
}

/// Builds one media-group item around an already-selected file: the per-kind
/// `InputMedia` (same spoiler/caption handling) plus the video's thumbnail,
/// which Telegram takes as a separate upload/URL. The one place that dispatch
/// is written; callers only choose the `InputFile`.
pub(super) fn media_from(
    item: &MediaItemPayload,
    file: InputFile,
    caption: Option<&str>,
    thumbnail: Option<&str>,
) -> Result<InputMedia, String> {
    let media = match item {
        MediaItemPayload::Photo { has_spoiler, .. } => photo_media(file, caption, *has_spoiler),
        MediaItemPayload::Video { has_spoiler, .. } => video_media(file, caption, *has_spoiler),
        MediaItemPayload::Animation { has_spoiler, .. } => {
            animation_media(file, caption, *has_spoiler)
        }
    };
    match (thumbnail, media) {
        (Some(thumb), InputMedia::Video(video)) => {
            Ok(InputMedia::Video(video.thumbnail(input_file_for(thumb)?)))
        }
        (_, media) => Ok(media),
    }
}

/// Builds a media group from payloads; only the first item of the batch gets
/// the caption (Telegram rejects captions on later items).
pub(super) fn build_media_group(
    batch: &[MediaItemPayload],
    caption: Option<&str>,
) -> Result<Vec<InputMedia>, String> {
    batch
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let item_caption = if i == 0 { caption } else { None };
            media_from(item, item.input_file()?, item_caption, item.thumbnail_url())
        })
        .collect()
}
