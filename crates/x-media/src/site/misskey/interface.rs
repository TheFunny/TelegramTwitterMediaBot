//! Site adapter for misskey.io notes: URL pattern, API fetch and
//! normalization into [`Fetched`] (see [`crate::site::Site`]).

use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched, RenderData, Site, SiteFuture};
use html_escape::encode_text;
use regex::Regex;
use std::sync::LazyLock;

const API_URL: &str = "https://misskey.io/api/notes/show";

/// Registry entry for the misskey.io adapter (see [`crate::site::Site`]).
pub struct MisskeySite;

impl Site for MisskeySite {
    fn id(&self) -> &'static str {
        "misskey"
    }

    fn pattern(&self) -> &'static Regex {
        &PATTERN
    }

    fn cache_key(&self, url: &str) -> Option<String> {
        cache_key(url)
    }

    fn fetch_from_url<'a>(&'a self, url: &'a str) -> SiteFuture<'a, Fetched> {
        Box::pin(async move { fetch_from_url(url).await })
    }
}

pub static PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:https?://)?misskey\.io/notes/([\w.\-~]+)").unwrap());

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let caps = PATTERN.captures(url).ok_or(FetchError::NotFound)?;
    let note_id = caps.get(1).ok_or(FetchError::NotFound)?.as_str();
    let note = fetch(note_id).await?;
    Ok(note.into())
}

/// Cache key for a misskey URL: `"misskey:<note id>"`. The prefix is the
/// site id used for caption-format lookup and link-cache keys.
pub fn cache_key(url: &str) -> Option<String> {
    PATTERN
        .captures(url)
        .map(|caps| format!("misskey:{}", &caps[1]))
}

/// Fetches a note from misskey.io by id. The API answers client failures
/// with HTTP 400 + `{"error":{"code":...}}` (NO_SUCH_NOTE → NotFound);
/// everything else non-success is transient and retried by [`crate::site::fetch`].
pub async fn fetch(note_id: &str) -> Result<model::Note, FetchError> {
    let response = crate::site::CLIENT
        .post(API_URL)
        .json(&serde_json::json!({ "noteId": note_id }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            400 => not_found_or_invalid(response).await,
            // A refusal or an auth demand is not a bad moment.
            401 | 403 => FetchError::Blocked,
            _ => FetchError::Transient(format!("misskey status {status}")),
        });
    }
    response.json().await.map_err(|e| FetchError::Site {
        site: "misskey",
        error: Box::new(e),
    })
}

/// Maps a 400 response: NO_SUCH_NOTE is permanent NotFound, any other 400 is
/// a site error (permanent — retrying a rejected request cannot succeed).
async fn not_found_or_invalid(response: reqwest::Response) -> FetchError {
    match response.json::<serde_json::Value>().await {
        Ok(v) if v["error"]["code"] == "NO_SUCH_NOTE" => FetchError::NotFound,
        _ => FetchError::Site {
            site: "misskey",
            error: "note rejected (invalid param or private note)".into(),
        },
    }
}

/// The note whose content matters: a renote shell has no text/files of its
/// own — the embedded renote carries them.
fn effective(note: &model::Note) -> &model::Note {
    match &note.renote {
        Some(renote) if note.files.is_empty() => renote,
        _ => note,
    }
}

impl From<model::Note> for Fetched {
    fn from(note: model::Note) -> Self {
        let note = &note;
        let content = effective(note);
        let url = format!("https://misskey.io/notes/{}", note.id);
        let author = content
            .user
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(&content.user.username)
            .to_string();
        let author_url = format!("https://misskey.io/@{}", content.user.username);
        let cw = content.cw.as_deref().unwrap_or_default();
        // Notes carry hashtags inline in the text (no structured tags array);
        // a CW note gets the marker prefixed so recipients see the spoiler.
        let mut text = cw.to_string();
        if !cw.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(content.text.as_deref().unwrap_or_default().trim());
        let text = text.trim().to_string();

        let caption = crate::site::caption(&url, &author_url, &author, &text);
        let sensitive = content.cw.is_some() || content.files.iter().any(|f| f.is_sensitive);
        let media: Vec<Media> = content.files.iter().filter_map(media_from_file).collect();

        Fetched {
            source_url: url.clone(),
            caption,
            // A note has no title: its text (CW marker included) is content.
            title: String::new(),
            content: text.clone(),
            media,
            sensitive,
            site_id: "misskey",
            render_data: Some(RenderData {
                url,
                author: encode_text(&author).into_owned(),
                author_url: author_url.clone(),
                title: String::new(),
                content: encode_text(&text).into_owned(),
                tags: String::new(),
            }),
            _keep_alive: None,
        }
    }
}

/// Maps a Misskey DriveFile to a [`Media`] item; unknown/audio/other types
/// are skipped (twitter's `_ => {}` precedent). GIF must be matched before
/// the generic image arm.
fn media_from_file(file: &model::DriveFile) -> Option<Media> {
    match file.mime_type.as_str() {
        "image/gif" => Some(Media::Animated {
            url: file.url.clone(),
            thumbnail_url: file.thumbnail_url.clone().unwrap_or_default(),
        }),
        mime if mime.starts_with("image/") => Some(Media::Illustration {
            url: file.url.clone(),
            thumbnail_url: file.thumbnail_url.clone(),
            fallback_url: None,
        }),
        mime if mime.starts_with("video/") => Some(Media::Video {
            url: file.url.clone(),
            thumbnail_url: file.thumbnail_url.clone().unwrap_or_default(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_json(json: serde_json::Value) -> model::Note {
        serde_json::from_value(json).unwrap()
    }

    fn base_note() -> serde_json::Value {
        serde_json::json!({
            "id": "aotihl10lqrs015s",
            "text": "hello",
            "user": { "name": "ミロン", "username": "donyan47897", "host": null },
            "files": []
        })
    }

    #[test]
    fn pattern_matches_misskey_note_urls() {
        for url in [
            "https://misskey.io/notes/aotihl10lqrs015s",
            "http://misskey.io/notes/aotihl10lqrs015s",
            "misskey.io/notes/aotihl10lqrs015s",
        ] {
            assert!(PATTERN.is_match(url), "{url}");
        }
        for url in [
            "https://misskey.io/",
            "https://misskey.io/@user",
            "https://misskey.io/notes/",
            "https://x.com/user/status/123",
        ] {
            assert!(!PATTERN.is_match(url), "{url}");
        }
    }

    #[test]
    fn cache_key_normalizes_variants() {
        assert_eq!(
            cache_key("https://misskey.io/notes/aotihl10lqrs015s"),
            Some("misskey:aotihl10lqrs015s".to_string())
        );
        assert_eq!(crate::site::site_id_from_key("misskey:abc"), "misskey");
    }

    #[test]
    fn from_json_image_file() {
        let mut note = base_note();
        note["files"] = serde_json::json!([{
            "type": "image/webp",
            "url": "https://media.misskeyusercontent.jp/io/a.webp",
            "thumbnailUrl": "https://media.misskeyusercontent.jp/io/t.webp",
            "isSensitive": true,
            "name": "pic.webp"
        }]);
        let fetched: Fetched = note_json(note).into();
        assert_eq!(
            fetched.source_url,
            "https://misskey.io/notes/aotihl10lqrs015s"
        );
        assert_eq!(fetched.site_id, "misskey");
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "hello");
        assert!(fetched.sensitive);
        assert_eq!(fetched.media.len(), 1);
        match &fetched.media[0] {
            Media::Illustration {
                url,
                thumbnail_url,
                fallback_url,
            } => {
                assert_eq!(url, "https://media.misskeyusercontent.jp/io/a.webp");
                assert_eq!(
                    thumbnail_url.as_deref(),
                    Some("https://media.misskeyusercontent.jp/io/t.webp")
                );
                assert!(fallback_url.is_none());
            }
            other => panic!("expected illustration, got {other:?}"),
        }
    }

    #[test]
    fn from_json_gif_video_and_skip_audio() {
        let mut note = base_note();
        note["files"] = serde_json::json!([
            { "type": "audio/mpeg", "url": "https://m/a.mp3", "isSensitive": false },
            { "type": "image/gif", "url": "https://m/a.gif", "isSensitive": false },
            { "type": "video/webm", "url": "https://m/a.webm", "isSensitive": false }
        ]);
        let fetched: Fetched = note_json(note).into();
        assert_eq!(fetched.media.len(), 2);
        assert!(
            matches!(&fetched.media[0], Media::Animated { url, .. } if url == "https://m/a.gif")
        );
        assert!(matches!(&fetched.media[1], Media::Video { url, .. } if url == "https://m/a.webm"));
        // No thumbnailUrl → empty string, not a broken URL.
        match &fetched.media[1] {
            Media::Video { thumbnail_url, .. } => assert_eq!(thumbnail_url, ""),
            other => panic!("expected video, got {other:?}"),
        }
        assert!(!fetched.sensitive);
    }

    #[test]
    fn from_json_cw_marks_sensitive_and_prefixes_title() {
        let mut note = base_note();
        note["cw"] = serde_json::json!("spoiler");
        note["text"] = serde_json::json!("body");
        let fetched: Fetched = note_json(note).into();
        assert!(fetched.sensitive);
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "spoiler body");
    }

    #[test]
    fn from_json_author_falls_back_to_username() {
        let mut note = base_note();
        note["user"] = serde_json::json!({ "name": null, "username": "donyan47897", "host": null });
        let fetched: Fetched = note_json(note).into();
        assert!(
            fetched.caption.contains("donyan47897"),
            "{}",
            fetched.caption
        );
        assert!(fetched.caption.contains("https://misskey.io/@donyan47897"));
    }

    #[test]
    fn from_json_renote_uses_embedded_content() {
        let note = serde_json::json!({
            "id": "shell0000000000",
            "text": null,
            "user": { "name": "shell", "username": "shelluser", "host": null },
            "files": [],
            "renote": {
                "id": "inner000000000",
                "text": "inner text",
                "user": { "name": "inner", "username": "inneruser", "host": null },
                "files": [
                    { "type": "image/png", "url": "https://m/i.png", "isSensitive": false }
                ]
            }
        });
        let fetched: Fetched = note_json(note).into();
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "inner text");
        assert_eq!(fetched.media.len(), 1);
        // The source URL still points at the renote shell the user posted.
        assert_eq!(
            fetched.source_url,
            "https://misskey.io/notes/shell0000000000"
        );
    }

    #[test]
    fn caption_layout_matches_bsky() {
        let fetched: Fetched = note_json(base_note()).into();
        assert_eq!(
            fetched.caption,
            "https://misskey.io/notes/aotihl10lqrs015s\n<a href=\"https://misskey.io/@donyan47897\">ミロン</a>: hello"
        );
    }

    #[test]
    fn caption_without_text_has_no_dangling_colon() {
        let mut note = base_note();
        note["text"] = serde_json::json!(null);
        let fetched: Fetched = note_json(note).into();
        assert_eq!(
            fetched.caption,
            "https://misskey.io/notes/aotihl10lqrs015s\n<a href=\"https://misskey.io/@donyan47897\">ミロン</a>"
        );
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to misskey.io"]
    async fn live_fetch_reference_note() {
        let fetched = fetch_from_url("https://misskey.io/notes/aotihl10lqrs015s")
            .await
            .unwrap();
        assert_eq!(fetched.site_id, "misskey");
        assert_eq!(fetched.media.len(), 1);
        assert!(fetched.sensitive);
        assert!(!fetched.caption.is_empty());
    }
}
