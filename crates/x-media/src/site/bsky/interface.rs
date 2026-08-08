use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched};
use html_escape::encode_text;
use regex::Regex;
use std::sync::LazyLock;

pub static PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"bsky\.app/profile/([\w.\-:]+)/post/([\w.\-~]+)").unwrap());

pub fn enabled() -> bool {
    true
}

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let caps = PATTERN.captures(url).ok_or(FetchError::NotFound)?;
    let handle = caps
        .get(1)
        .map(|m| m.as_str())
        .ok_or(FetchError::NotFound)?;
    let rkey = caps
        .get(2)
        .map(|m| m.as_str())
        .ok_or(FetchError::NotFound)?;
    let post = fetch(handle, rkey).await?;
    let mut fetched: Fetched = post.into();
    // bsky video embeds expose only an HLS playlist URL, which Telegram
    // cannot fetch; remux it to a single MP4 (mirrors the pixiv ugoira
    // encode path — the temp file stays alive via `_keep_alive`). On any
    // failure the video item is dropped and the post degrades to its text.
    let mut media = Vec::with_capacity(fetched.media.len());
    for item in fetched.media {
        let is_hls = matches!(&item, Media::Video { url, .. }
            if url.contains("playlist") || url.ends_with(".m3u8"));
        if !is_hls {
            media.push(item);
            continue;
        }
        let url = item.url().to_string();
        match resolve_bsky_video(&url).await {
            Ok(Some((mp4_path, keep_alive))) => {
                let thumbnail_url = match &item {
                    Media::Video { thumbnail_url, .. } => thumbnail_url.clone(),
                    _ => String::new(),
                };
                media.push(Media::Video {
                    title: None,
                    url: mp4_path.to_string_lossy().into_owned(),
                    thumbnail_url,
                });
                fetched._keep_alive = Some(keep_alive);
            }
            Ok(None) => log::warn!("bsky video remux unavailable for {url}"),
            Err(e) => log::warn!("bsky video remux failed for {url}: {e}"),
        }
    }
    fetched.media = media;
    Ok(fetched)
}

/// Downloads an HLS playlist (master or media) and remuxes its segments to a
/// single MP4 via ffmpeg. Returns the MP4 path plus the temp dir that must
/// stay alive until the file is uploaded. `Ok(None)` when ffmpeg is missing.
///
/// Verified live (2026-08): bsky master playlists carry `#EXT-X-STREAM-INF`
/// variant lines (e.g. `720p/video.m3u8?session_id=…`), and the media
/// playlists are VOD MPEG-TS segments (`videoN.ts?…`) without EXT-X-MAP, so
/// a plain `-f concat -c copy` remux is valid.
async fn resolve_bsky_video(
    playlist_url: &str,
) -> Result<Option<(std::path::PathBuf, tempfile::TempDir)>, String> {
    if !crate::site::ffmpeg_available() {
        crate::site::log_once_ffmpeg_missing();
        return Ok(None);
    }
    let master = crate::site::download_media_limited(playlist_url, 1_048_576)
        .await
        .map_err(|e| format!("bsky video master playlist: {e}"))?;
    let master = String::from_utf8_lossy(&master);

    // Master playlist: pick the variant with the highest declared bandwidth.
    let playlist_url = if master.contains("#EXT-X-STREAM-INF") {
        let mut best: Option<(u64, String)> = None;
        let mut lines = master.lines();
        while let Some(line) = lines.next() {
            if !line.starts_with("#EXT-X-STREAM-INF") {
                continue;
            }
            let bandwidth = line
                .split_once("BANDWIDTH=")
                .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            if let Some(uri) = lines.next().filter(|u| !u.starts_with('#')) {
                if bandwidth >= best.as_ref().map(|(b, _)| *b).unwrap_or(0) {
                    best = Some((bandwidth, uri.to_string()));
                }
            }
        }
        let Some((_, uri)) = best else {
            return Err("bsky video master playlist has no variants".to_string());
        };
        url::Url::parse(playlist_url)
            .and_then(|base| base.join(&uri))
            .map_err(|e| format!("bsky video variant URL: {e}"))?
            .to_string()
    } else {
        playlist_url.to_string()
    };

    let variant = crate::site::download_media_limited(&playlist_url, 1_048_576)
        .await
        .map_err(|e| format!("bsky video media playlist: {e}"))?;
    let variant = String::from_utf8_lossy(&variant);
    // Segment URIs: non-#, non-empty lines, resolved relative to the playlist.
    let base = url::Url::parse(&playlist_url).map_err(|e| format!("bsky playlist URL: {e}"))?;
    let segments: Vec<String> = variant
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| base.join(l).map(|u| u.to_string()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("bsky segment URL: {e}"))?;
    if segments.is_empty() {
        return Err("bsky video playlist has no segments".to_string());
    }
    if segments.len() > 500 {
        return Err("bsky video has too many segments".to_string());
    }

    let frames_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let out_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut total: u64 = 0;
    let mut list = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let bytes = crate::site::download_media_limited(seg, 20 * 1024 * 1024)
            .await
            .map_err(|e| format!("bsky segment {i}: {e}"))?;
        total += bytes.len() as u64;
        if total > 256 * 1024 * 1024 {
            return Err("bsky video exceeds total size cap".to_string());
        }
        let path = frames_dir.path().join(format!("seg_{i:04}.ts"));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        list.push_str(&format!("file '{}'\n", path.to_string_lossy()));
    }
    let list_path = frames_dir.path().join("list.txt");
    std::fs::write(&list_path, &list).map_err(|e| e.to_string())?;

    let output = out_dir.path().join("video.mp4");
    let list_str = list_path.to_string_lossy().into_owned();
    let output_str = output.to_string_lossy().into_owned();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                &list_str,
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                &output_str,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    })
    .await
    .map_err(|e| format!("bsky remux worker panicked: {e}"))?;
    match status {
        Ok(s) if s.success() => Ok(Some((output, out_dir))),
        Ok(s) => Err(format!("ffmpeg exited with {s}")),
        Err(e) => Err(format!("ffmpeg spawn failed: {e}")),
    }
}

/// Fetches a post thread by handle or DID (`at://` URIs work for both).
pub async fn fetch(handle: &str, rkey: &str) -> Result<Post, FetchError> {
    let response = crate::site::CLIENT
        .get(API_URL)
        .query(&[
            ("uri", format!("at://{handle}/app.bsky.feed.post/{rkey}")),
            ("depth", "0".to_string()),
        ])
        .send()
        .await?;
    // 404/410 = gone (permanent); 429/5xx = transient and retried by fetch.
    let status = response.status();
    if !status.is_success() {
        return match status.as_u16() {
            404 | 410 => Err(FetchError::NotFound),
            _ => Err(FetchError::Transient(format!("bsky status {status}"))),
        };
    }
    let text = response.text().await?;
    Ok(Post::from_json(&text, rkey.to_string())?)
}

#[derive(Debug)]
pub struct Post {
    id: String,
    author: String,
    author_id: String,
    text: String,
    media: Vec<Media>,
    sensitive: bool,
}

impl Post {
    fn url(&self) -> String {
        format!("{}/post/{}", self.author_url(), self.id)
    }

    fn author_url(&self) -> String {
        format!("https://bsky.app/profile/{}", self.author_id)
    }

    pub fn caption(&self) -> String {
        format!(
            "{url}\n<a href=\"{author_url}\">{author}</a>: {text}",
            url = self.url(),
            author_url = self.author_url(),
            author = encode_text(&self.author),
            text = encode_text(&self.text),
        )
    }

    pub fn from_json(raw_json: &str, id: String) -> Result<Self, FetchError> {
        let json: serde_json::Value = serde_json::from_str(raw_json).map_err(FetchError::Json)?;
        let json: model::Info = serde_json::from_value(json).map_err(FetchError::Json)?;
        match json.thread {
            model::Thread::Post { post } => {
                let text = post.record.text;
                let author = post.author.display_name.unwrap_or_default();
                let author_id = post.author.handle;
                let mut media = vec![];
                if let Some(embed) = post.embed {
                    match embed {
                        model::Media::Images { images } => {
                            media.extend(images.into_iter().map(|image| Media::Illustration {
                                title: None,
                                url: image.fullsize,
                                thumbnail_url: Some(image.thumb),
                                fallback_url: None,
                            }));
                        }
                        model::Media::Video {
                            playlist,
                            thumbnail,
                        } => {
                            media.push(Media::Video {
                                title: None,
                                url: playlist,
                                thumbnail_url: thumbnail,
                            });
                        }
                        model::Media::External => {}
                    }
                }
                let sensitive = post
                    .labels
                    .iter()
                    .any(|label| SENSITIVE_LABEL.contains(&label.val.as_str()));
                Ok(Post {
                    id,
                    author,
                    author_id,
                    text,
                    media,
                    sensitive,
                })
            }
            model::Thread::NotFound => Err(FetchError::NotFound),
            model::Thread::Blocked => Err(FetchError::Blocked),
        }
    }
}

impl From<Post> for Fetched {
    fn from(post: Post) -> Self {
        let url = post.url();
        let author_url = post.author_url();
        let render_data = Some(crate::site::RenderData {
            url: url.clone(),
            author: encode_text(&post.author).into_owned(),
            author_url: author_url.clone(),
            title: encode_text(&post.text).into_owned(),
            tags: String::new(),
        });
        Fetched {
            source_url: url,
            caption: post.caption(),
            title: post.text.clone(),
            media: post.media,
            sensitive: post.sensitive,
            render_data,
            _keep_alive: None,
        }
    }
}

const API_URL: &str = "https://public.api.bsky.app/xrpc/app.bsky.feed.getPostThread";
const SENSITIVE_LABEL: [&str; 4] = ["sexual", "nudity", "porn", "graphic-media"];

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_json(post_json: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "thread": post_json })
    }

    #[test]
    fn pattern_matches_handle_and_did() {
        let cases = [
            (
                "https://bsky.app/profile/user.bsky.social/post/3laoveufjv224",
                "user.bsky.social",
                "3laoveufjv224",
            ),
            (
                "https://bsky.app/profile/did:plc:abc123def/post/3xxxx",
                "did:plc:abc123def",
                "3xxxx",
            ),
        ];
        for (url, handle, rkey) in cases {
            let caps = PATTERN.captures(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(caps.get(1).unwrap().as_str(), handle);
            assert_eq!(caps.get(2).unwrap().as_str(), rkey);
        }
    }

    #[test]
    fn pattern_rejects_non_post_urls() {
        for url in [
            "https://bsky.app/profile/user.bsky.social",
            "https://bsky.app/profile/user.bsky.social/posts",
            "https://x.com/user/status/123",
        ] {
            assert!(!PATTERN.is_match(url), "{url}");
        }
    }

    #[test]
    fn from_json_images_with_missing_defaults() {
        let raw = thread_json(serde_json::json!({
            "$type": "app.bsky.feed.defs#threadViewPost",
            "post": {
                "author": { "handle": "user.bsky.social" },
                "record": { "$type": "app.bsky.feed.post", "text": "hello <world>" },
                "embed": {
                    "$type": "app.bsky.embed.images#view",
                    "images": [
                        { "thumb": "https://cdn.bsky.app/img/thumb", "fullsize": "https://cdn.bsky.app/img/full", "alt": "" }
                    ]
                }
            }
        }));
        let post = Post::from_json(&raw.to_string(), "3xxxx".into()).unwrap();
        let fetched: Fetched = post.into();
        assert_eq!(
            fetched.source_url,
            "https://bsky.app/profile/user.bsky.social/post/3xxxx"
        );
        assert_eq!(fetched.title, "hello <world>");
        assert_eq!(fetched.media.len(), 1);
        assert!(!fetched.sensitive);
        // display_name absent -> empty fallback
        assert!(
            fetched.caption.contains("</a>: hello &lt;world&gt;"),
            "caption: {}",
            fetched.caption
        );
    }

    #[test]
    fn from_json_sensitive_labels() {
        let raw = thread_json(serde_json::json!({
            "$type": "app.bsky.feed.defs#threadViewPost",
            "post": {
                "author": { "handle": "u.bsky.social", "displayName": "U" },
                "record": { "$type": "app.bsky.feed.post", "text": "x" },
                "labels": [{ "val": "porn" }]
            }
        }));
        let post = Post::from_json(&raw.to_string(), "3xxxx".into()).unwrap();
        assert!(post.sensitive);
    }

    #[test]
    fn from_json_blocked_and_not_found() {
        let blocked = thread_json(serde_json::json!({
            "$type": "app.bsky.feed.defs#blockedPost",
            "blocked": true
        }));
        assert!(matches!(
            Post::from_json(&blocked.to_string(), "3xxxx".into()),
            Err(FetchError::Blocked)
        ));

        let not_found = thread_json(serde_json::json!({
            "$type": "app.bsky.feed.defs#notFoundPost",
            "notFound": true
        }));
        assert!(matches!(
            Post::from_json(&not_found.to_string(), "3xxxx".into()),
            Err(FetchError::NotFound)
        ));
    }

    #[tokio::test]
    async fn live_fetch_with_photos() {
        let fetched =
            fetch_from_url("https://bsky.app/profile/asagi0398.bsky.social/post/3mqkhrq5w6k2m")
                .await
                .unwrap();
        assert_eq!(
            fetched.source_url,
            "https://bsky.app/profile/asagi0398.bsky.social/post/3mqkhrq5w6k2m"
        );
        assert!(!fetched.caption.is_empty());
    }

    #[tokio::test]
    async fn live_fetch_smoke() {
        let fetched =
            fetch_from_url("https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224")
                .await
                .unwrap();
        assert_eq!(
            fetched.source_url,
            "https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224"
        );
        assert!(!fetched.caption.is_empty());
    }
}
