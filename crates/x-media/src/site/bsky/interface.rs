use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched, Site, SiteFuture};
use html_escape::{encode_double_quoted_attribute, encode_text};
use regex::Regex;
use std::sync::LazyLock;

/// Registry entry for the bluesky adapter (see [`crate::site::Site`]).
pub struct BskySite;

impl Site for BskySite {
    fn id(&self) -> &'static str {
        "bsky"
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

pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?://)?bsky\.app/profile/([\w.\-:]+)/post/([\w.\-~]+)").unwrap()
});

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
    // The remux warnings below name the post, not the CDN URL they were
    // working on: the media URL is derived from what the user pasted, and
    // `warn` is a level operators share.
    let key = cache_key(url).unwrap_or_else(|| "?".into());
    // A failed remux is remembered: if it leaves the post with no media at
    // all, returning `Ok` would read as "this post has no media". It is
    // reported as `FetchError::MediaPrep` rather than a transient failure —
    // the download legs already got their own retry in place ([`fetch_hls`]),
    // and the fetch loop's retry would only download every segment again to
    // fail the same way.
    let mut remux_failure: Option<String> = None;
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
                    url: mp4_path.to_string_lossy().into_owned(),
                    thumbnail_url,
                });
                fetched._keep_alive = Some(std::sync::Arc::new(keep_alive));
            }
            // No ffmpeg: a deployment gap, not a bad moment — retrying it
            // would only waste the fetch budget, so the post degrades (and an
            // all-video post reports the media type as unsupported).
            Ok(None) => log::warn!("bsky video remux unavailable for [key={key}]"),
            Err(e) => {
                log::warn!("bsky video remux failed for [key={key}]: {e}");
                remux_failure = Some(e);
            }
        }
    }
    if media.is_empty()
        && let Some(reason) = remux_failure
    {
        return Err(FetchError::MediaPrep(format!(
            "bsky video remux failed: {reason}"
        )));
    }
    fetched.media = media;
    Ok(fetched)
}

/// Cache key for a bsky URL: `"bsky:<handle>/<rkey>"`. The prefix is the
/// site id used for caption-format lookup and link-cache keys.
pub fn cache_key(url: &str) -> Option<String> {
    PATTERN
        .captures(url)
        .map(|caps| format!("bsky:{}/{}", &caps[1], &caps[2]))
}

/// Segments fetched (and written) at once while remuxing an HLS video. Small
/// on purpose: a segment can be up to 20 MiB and the whole playlist is capped
/// at 256 MiB, so this is also what bounds the remux's peak memory.
const SEGMENT_CONCURRENCY: usize = 4;

/// The ffmpeg concat list for the downloaded segments, **in segment order**.
/// The downloads complete in completion order (`JoinSet`), and ffmpeg would
/// happily concatenate them in whatever order the list holds: an out-of-order
/// list produces a silently scrambled video, not an error.
fn concat_list(files: &mut [(usize, std::path::PathBuf)]) -> String {
    files.sort_by_key(|(i, _)| *i);
    files
        .iter()
        .map(|(_, path)| format!("file '{}'\n", path.to_string_lossy()))
        .collect()
}

/// One HLS fetch (a playlist or a segment) with an in-place retry for a
/// retryable class (transport, 429/5xx). These used to get their retry from the
/// outer fetch loop, which pays for it by replaying the whole post: master
/// playlist, variant playlist and every segment again. A segment failing near
/// the end of a 500-segment video meant downloading the entire thing twice
/// more, so the second attempt belongs on the request that actually failed.
async fn fetch_hls(url: &str, cap: u64) -> Result<bytes::Bytes, String> {
    match crate::site::download_media_limited(url, cap, crate::site::DOWNLOAD_TOTAL_TIMEOUT).await {
        Err(FetchError::Http(_) | FetchError::Transient(_)) => {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            crate::site::download_media_limited(url, cap, crate::site::DOWNLOAD_TOTAL_TIMEOUT)
                .await
                .map_err(|e| e.to_string())
        }
        other => other.map_err(|e| e.to_string()),
    }
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
    let master = fetch_hls(playlist_url, 1_048_576)
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
            if let Some(uri) = lines.next().filter(|u| !u.starts_with('#'))
                && bandwidth >= best.as_ref().map(|(b, _)| *b).unwrap_or(0)
            {
                best = Some((bandwidth, uri.to_string()));
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

    let variant = fetch_hls(&playlist_url, 1_048_576)
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

    let frames_dir = tempfile::Builder::new()
        .prefix(crate::TEMP_FILE_PREFIX)
        .tempdir()
        .map_err(|e| e.to_string())?;
    let out_dir = tempfile::Builder::new()
        .prefix(crate::TEMP_FILE_PREFIX)
        .tempdir()
        .map_err(|e| e.to_string())?;
    // Segments are fetched concurrently under a small bound, and written with
    // `tokio::fs` (a multi-megabyte `std::fs::write` blocks the executor
    // thread). Serially, a several-hundred-segment video made the user wait
    // for every round trip in turn — the dominant cost of a remux.
    let mut total: u64 = 0;
    let mut written: Vec<(usize, std::path::PathBuf)> = Vec::with_capacity(segments.len());
    let mut next = 0;
    let mut set = tokio::task::JoinSet::new();
    loop {
        while set.len() < SEGMENT_CONCURRENCY && next < segments.len() {
            let i = next;
            next += 1;
            let seg = segments[i].clone();
            let path = frames_dir.path().join(format!("seg_{i:04}.ts"));
            set.spawn(async move {
                let bytes = fetch_hls(&seg, 20 * 1024 * 1024)
                    .await
                    .map_err(|e| format!("bsky segment {i}: {e}"))?;
                tokio::fs::write(&path, &bytes)
                    .await
                    .map_err(|e| format!("bsky segment {i}: {e}"))?;
                Ok::<_, String>((i, bytes.len() as u64, path))
            });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (i, len, path) = joined.map_err(|e| format!("bsky segment task panicked: {e}"))??;
        total += len;
        if total > 256 * 1024 * 1024 {
            return Err("bsky video exceeds total size cap".to_string());
        }
        written.push((i, path));
    }
    let list = concat_list(&mut written);
    let list_path = frames_dir.path().join("list.txt");
    tokio::fs::write(&list_path, &list)
        .await
        .map_err(|e| e.to_string())?;

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
        return Err(crate::site::status_error("bsky", status));
    }
    let text = response.text().await?;
    Post::from_json(&text, rkey.to_string())
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
            url = encode_double_quoted_attribute(&self.url()),
            author_url = encode_double_quoted_attribute(&self.author_url()),
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
            // A post has no title: its text is all content.
            title: String::new(),
            content: encode_text(&post.text).into_owned(),
            tags: String::new(),
        });
        Fetched {
            source_url: url,
            caption: post.caption(),
            title: String::new(),
            content: post.text.clone(),
            media: post.media,
            sensitive: post.sensitive,
            site_id: "bsky",
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

    /// The downloads finish in completion order; ffmpeg concatenates whatever
    /// order `list.txt` holds, so an unsorted list is a scrambled video rather
    /// than an error.
    #[test]
    fn concat_list_is_in_segment_order() {
        let mut files = vec![
            (2, std::path::PathBuf::from("/t/seg_0002.ts")),
            (0, std::path::PathBuf::from("/t/seg_0000.ts")),
            (1, std::path::PathBuf::from("/t/seg_0001.ts")),
        ];
        assert_eq!(
            concat_list(&mut files),
            "file '/t/seg_0000.ts'\nfile '/t/seg_0001.ts'\nfile '/t/seg_0002.ts'\n"
        );
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

    /// A remux failure is a `MediaPrep`, which the fetch loop does not retry:
    /// replaying the post means downloading every HLS segment again, when the
    /// request that failed already got its second attempt in place
    /// ([`fetch_hls`]). The classes below are the ones still retried there.
    #[test]
    fn media_prep_failure_is_not_retried() {
        use crate::site::Site as _;
        assert!(!BskySite.is_retryable(&FetchError::MediaPrep(
            "bsky video remux failed: segment 400: 503".into()
        )));
        assert!(BskySite.is_retryable(&FetchError::Transient("429".into())));
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
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "hello <world>");
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

    /// The one live bsky check: a labelled post with photos — source URL,
    /// caption, media and the sensitive label all survive the parse. This
    /// replaced a second byte-identical live test whose URL is a *text-only*
    /// post, so neither copy pinned any media.
    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to public.api.bsky.app"]
    async fn live_fetch_with_photos() {
        let url = "https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224";
        let fetched = fetch_from_url(url).await.unwrap();
        assert_eq!(fetched.source_url, url);
        assert!(!fetched.caption.is_empty());
        assert!(!fetched.media.is_empty(), "expected photos in {url}");
        assert!(fetched.sensitive, "expected a label on {url}");
    }
}
