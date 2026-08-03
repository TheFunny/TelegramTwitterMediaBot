//! Site fetching dispatcher and unified result types.
//!
//! Dispatch order: twitter → bsky → pixiv. Each site module exports a
//! `PATTERN`, `enabled()` and `fetch_from_url()`; a future site plugs in by
//! adding one guarded entry in [`fetch_once`].

use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

pub mod bsky;
pub mod pixiv;
pub mod twitter;

pub use pixiv::PixivError;

/// The result of fetching a post: canonical URL, HTML caption, raw text,
/// media list and spoiler flag. Produced by [`fetch`].
#[derive(Debug)]
pub struct Fetched {
    /// Canonical URL: x.com/{author}/status/{id} |
    /// https://www.pixiv.net/artworks/{id} |
    /// https://bsky.app/profile/{handle}/post/{rkey}
    pub source_url: String,
    /// The exact HTML produced by the site's caption().
    pub caption: String,
    /// Raw post text (tweet text / bsky text / pixiv title).
    pub title: String,
    pub media: Vec<crate::media::Media>,
    /// Spoiler flag for all media of this post.
    pub sensitive: bool,
    /// Raw values (pre-escaped) for user-customizable caption formats.
    pub(crate) render_data: Option<RenderData>,
    /// Keeps temp files (e.g. an encoded ugoira MP4) alive until the caller
    /// finishes uploading; not part of the public contract.
    pub(crate) _keep_alive: Option<tempfile::TempDir>,
}

/// Pre-escaped values for `{url} {author} {author_url} {title} {tags}`
/// placeholders in user-supplied caption formats.
#[derive(Debug)]
pub(crate) struct RenderData {
    pub url: String,
    pub author: String,
    pub author_url: String,
    pub title: String,
    pub tags: String,
}

impl Fetched {
    /// The site this post came from (used for per-site format overrides).
    pub fn site_name(&self) -> &'static str {
        if self.source_url.contains("x.com") || self.source_url.contains("twitter.com") {
            "twitter"
        } else if self.source_url.contains("bsky.app") {
            "bsky"
        } else if self.source_url.contains("pixiv.net") {
            "pixiv"
        } else {
            "unknown"
        }
    }

    /// Renders a user-supplied caption format. The format string is
    /// HTML-escaped in full, then the (already-escaped) placeholder values
    /// are substituted — users can structure text but never inject raw HTML
    /// or attributes. An empty/unknown format falls back to the built-in
    /// caption.
    pub fn caption_with(&self, format: &str) -> String {
        match (&self.render_data, format.is_empty()) {
            (Some(data), false) => {
                let escaped = html_escape::encode_text(format).into_owned();
                escaped
                    .replace("{url}", &data.url)
                    .replace("{author}", &data.author)
                    .replace("{author_url}", &data.author_url)
                    .replace("{title}", &data.title)
                    .replace("{tags}", &data.tags)
            }
            _ => self.caption.clone(),
        }
    }
}

#[derive(Debug)]
pub enum FetchError {
    Http(reqwest::Error),
    Json(serde_json::Error),
    Pixiv(PixivError),
    NotFound,
    Blocked,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "http error: {e}"),
            FetchError::Json(e) => write!(f, "json error: {e}"),
            FetchError::Pixiv(e) => write!(f, "pixiv error: {e}"),
            FetchError::NotFound => write!(f, "not found"),
            FetchError::Blocked => write!(f, "blocked"),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FetchError::Http(e) => Some(e),
            FetchError::Json(e) => Some(e),
            FetchError::Pixiv(e) => Some(e),
            FetchError::NotFound | FetchError::Blocked => None,
        }
    }
}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        FetchError::Http(e)
    }
}


impl From<serde_json::Error> for FetchError {
    fn from(e: serde_json::Error) -> Self {
        FetchError::Json(e)
    }
}

impl From<PixivError> for FetchError {
    fn from(e: PixivError) -> Self {
        FetchError::Pixiv(e)
    }
}

/// Shared HTTP client (browser User-Agent) for twitter/bsky fetches and
/// [`download_media`].
pub(crate) static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let builder = reqwest::Client::builder().user_agent("Mozilla/5.0");
    // Each `#[tokio::test]` runs on its own runtime; the connection pool is
    // bound to the runtime that created it, so cross-runtime reuse of idle
    // connections fails with DispatchGone. In test builds every request uses
    // a fresh connection. Production runs on one runtime and keeps pooling.
    #[cfg(test)]
    let builder = builder.pool_max_idle_per_host(0);
    builder.build().expect("failed to build HTTP client")
});

/// Fetches a post from its URL. Returns `Ok(None)` when no site pattern
/// matches (unsupported links are silently ignored by the bot).
///
/// Transient network failures are retried: 3 total attempts with 1s then 2s
/// delays. Non-Http errors (Json/NotFound/Blocked/Pixiv) are not retried.
pub async fn fetch(url: &str) -> Result<Option<Fetched>, FetchError> {
    let mut last_http_error = None;
    for attempt in 0..3u32 {
        match fetch_once(url).await {
            Ok(Some(fetched)) => {
                log::info!(
                    "fetched {url}: site {} returned {} media",
                    fetched.site_name(),
                    fetched.media.len()
                );
                return Ok(Some(fetched));
            }
            Ok(None) => return Ok(None),
            Err(FetchError::Http(e)) => {
                last_http_error = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
            Err(other) => return Err(other),
        }
    }
    Err(FetchError::Http(
        last_http_error.expect("retry loop always ran 3 attempts"),
    ))
}

async fn fetch_once(url: &str) -> Result<Option<Fetched>, FetchError> {
    if twitter::enabled() && twitter::PATTERN.is_match(url) {
        return Ok(Some(twitter::fetch_from_url(url).await?));
    }
    if bsky::enabled() && bsky::PATTERN.is_match(url) {
        return Ok(Some(bsky::fetch_from_url(url).await?));
    }
    if pixiv::enabled() && pixiv::PATTERN.is_match(url) {
        return Ok(Some(pixiv::fetch_from_url(url).await?));
    }
    Ok(None)
}

/// Downloads media bytes for the bot's upload fallback: when Telegram's own
/// fetch of a media URL is blocked (hotlink protection), the bot downloads
/// the file itself and uploads it via multipart. Site-appropriate headers:
/// pixiv image hosts need the `Referer` header.
pub async fn download_media(url: &str) -> Result<bytes::Bytes, FetchError> {
    let mut request = CLIENT.get(url);
    let lower = url.to_ascii_lowercase();
    if lower.contains("pximg.net") {
        request = request.header("Referer", "https://www.pixiv.net/");
    }
    let response = request.send().await?;
    Ok(response.bytes().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_url_returns_none() {
        let result = fetch("https://example.com/some/article").await;
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    #[tokio::test]
    async fn unknown_scheme_returns_none() {
        let result = fetch("not a url at all").await;
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    #[tokio::test]
    async fn download_media_pixiv_original_with_referer() {
        // Proves the Referer header is attached for i.pximg.net: a header-less
        // GET to a pixiv original URL is rejected with 403.
        if std::env::var("PIXIV_REFRESH_TOKEN").is_err() {
            eprintln!("skipping: no PIXIV_REFRESH_TOKEN");
            return;
        }
        let illustration = pixiv::fetch(126839080).await.unwrap();
        let fetched: Fetched = illustration.into();
        let url = match fetched.media.first() {
            Some(crate::media::Media::Illustration { url, .. }) => url.clone(),
            other => panic!("expected illustration media, got {other:?}"),
        };
        assert!(url.contains("i.pximg.net"));
        let bytes = download_media(&url).await.unwrap();
        assert!(!bytes.is_empty());
    }
}
