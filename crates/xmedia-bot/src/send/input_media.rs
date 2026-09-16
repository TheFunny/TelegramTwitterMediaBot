//! Payload → Telegram input types: `InputFile` selection (cached file id /
//! URL / local path), the per-kind `InputMedia` builders and the media-group
//! assembly with its caption rule.

use super::MediaItemPayload;
use teloxide::types::{
    InputFile, InputMedia, InputMediaAnimation, InputMediaPhoto, InputMediaVideo, ParseMode,
};

fn parse_media_url(s: &str) -> Result<url::Url, String> {
    url::Url::parse(s).map_err(|e| format!("invalid media URL: {e}"))
}

pub(super) fn item_url(item: &MediaItemPayload) -> &str {
    match item {
        MediaItemPayload::Photo { media, .. }
        | MediaItemPayload::Video { media, .. }
        | MediaItemPayload::Animation { media, .. } => media,
    }
}

/// Remote http(s) URLs are handed to Telegram to fetch; everything else
/// (e.g. a locally encoded ugoira MP4) is uploaded directly.
pub(super) fn input_file_for(media: &str) -> Result<InputFile, String> {
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
