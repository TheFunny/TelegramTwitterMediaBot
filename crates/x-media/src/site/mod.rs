//! Site fetching dispatcher and unified result types.
//!
//! Dispatch order: twitter → bsky → pixiv. Each site module exports a
//! `PATTERN`, `enabled()` and `fetch_from_url()`; a future site plugs in by
//! adding one guarded entry in [`fetch_once`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;

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
    /// Site id (`"twitter"` / `"bsky"` / `"pixiv"`): the single source of
    /// truth for site identity — caption-format lookup, cache-key prefix and
    /// the SetFormat whitelist all derive from it. Set by the producing site.
    pub site_id: &'static str,
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
    /// A thin alias over [`Fetched::site_id`] kept for callers that read the
    /// site off a fetched post.
    pub fn site_name(&self) -> &'static str {
        self.site_id
    }

    /// Renders a user-supplied caption format. The format string is
    /// HTML-escaped in full, then the (already-escaped) placeholder values
    /// are substituted — users can structure text but never inject raw HTML
    /// or attributes. An empty/unknown format falls back to the built-in
    /// caption. The result is truncated to [`MAX_CAPTION_CHARS`] (Telegram's
    /// caption limit for HTML parse mode).
    pub fn caption_with(&self, format: &str) -> String {
        match (&self.render_data, format.is_empty()) {
            (Some(data), false) => caption_from_fields(
                format,
                "",
                &data.url,
                &data.author,
                &data.author_url,
                &data.title,
                &data.tags,
            ),
            _ => truncate_caption(&self.caption),
        }
    }

    /// The pre-escaped placeholder values (author, author_url, title, tags)
    /// a caller needs to rebuild a caption later, e.g. for a cached post
    /// where the [`Fetched`] is no longer available.
    pub fn render_fields(&self) -> Option<(&str, &str, &str, &str)> {
        self.render_data.as_ref().map(|d| {
            (
                d.author.as_str(),
                d.author_url.as_str(),
                d.title.as_str(),
                d.tags.as_str(),
            )
        })
    }

    /// Hands over the temp dir keeping locally produced media (ugoira MP4,
    /// bsky remux MP4) alive. The bot keeps it while its task may still be
    /// retried by the queue, which runs after this [`Fetched`] is dropped and
    /// its temp files would otherwise be gone. `None` when no such dir exists.
    pub fn take_keep_alive(&mut self) -> Option<tempfile::TempDir> {
        self._keep_alive.take()
    }
}

/// Telegram's caption length limit (chars) for HTML parse mode; longer
/// captions are rejected with a 400.
pub const MAX_CAPTION_CHARS: usize = 1024;

/// Truncates a caption to at most [`MAX_CAPTION_CHARS`] chars, appending an
/// ellipsis when cut. Backs off to before an unclosed HTML entity (`&amp`
/// without its `;` would be malformed HTML and rejected by Telegram).
pub fn truncate_caption(caption: &str) -> String {
    if caption.chars().count() <= MAX_CAPTION_CHARS {
        return caption.to_string();
    }
    // Leave one char for the ellipsis; floor_char_boundary lands on a char
    // edge (byte index ≤ MAX-1, so chars ≤ MAX-1).
    let mut end = caption.floor_char_boundary(MAX_CAPTION_CHARS - 1);
    // Don't split an entity: if the last '&' before `end` has no closing ';'
    // inside the kept part, cut before it.
    if let Some(amp) = caption[..end].rfind('&')
        && !caption[amp..end].contains(';')
    {
        end = amp;
    }
    let mut s = caption[..end].to_string();
    s.push('…');
    s
}

/// Renders a user-supplied caption format from raw (already-escaped) field
/// values with the same escaping/substitution rules as
/// [`Fetched::caption_with`]. An empty format returns `built_in` unchanged.
/// The result is truncated to [`MAX_CAPTION_CHARS`] (Telegram's caption
/// limit for HTML parse mode).
pub fn caption_from_fields(
    format: &str,
    built_in: &str,
    url: &str,
    author: &str,
    author_url: &str,
    title: &str,
    tags: &str,
) -> String {
    if format.is_empty() {
        return truncate_caption(built_in);
    }
    let escaped = html_escape::encode_text(format).into_owned();
    truncate_caption(
        &escaped
            .replace("{url}", url)
            .replace("{author}", author)
            .replace("{author_url}", author_url)
            .replace("{title}", title)
            .replace("{tags}", tags),
    )
}

/// Stable per-post cache key derived from any supported URL, so variant
/// domains (x.com / twitter.com / fxtwitter.com, mobile, `/photo/N`
/// suffixes) map to the same post. Delegates to each registered site's
/// `cache_key` (dispatch order twitter → bsky → pixiv).
pub fn cache_key(url: &str) -> Option<String> {
    SITES.iter().find_map(|site| site.cache_key(url))
}

/// The site id carried by a cache key (`"twitter:123"` → `"twitter"`).
/// Unknown prefixes fall back to `"unknown"`. The bot uses this on the
/// link-cache hit path, where no [`Fetched`] is available — the same value
/// a fresh fetch would read from [`Fetched::site_id`].
pub fn site_id_from_key(key: &str) -> &'static str {
    let prefix = key.split(':').next().unwrap_or("");
    SITES
        .iter()
        .map(|site| site.id())
        .find(|id| *id == prefix)
        .unwrap_or("unknown")
}

#[derive(Debug)]
pub enum FetchError {
    Http(reqwest::Error),
    Json(serde_json::Error),
    Pixiv(PixivError),
    NotFound,
    Blocked,
    /// The post exists but its content is withheld (twitter NSFW /
    /// age-restricted tweets come back as an empty `{}` from syndication).
    Sensitive,
    /// A download exceeded the caller's size cap (see [`download_media_limited`]).
    TooLarge,
    /// A transient server-side failure (429 / 5xx); [`fetch`] retries these.
    Transient(String),
    /// A local I/O failure while streaming a download to disk
    /// (see [`download_media_to_file`]).
    Io(std::io::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "http error: {e}"),
            FetchError::Json(e) => write!(f, "json error: {e}"),
            FetchError::Pixiv(e) => write!(f, "pixiv error: {e}"),
            FetchError::NotFound => write!(f, "not found"),
            FetchError::Blocked => write!(f, "blocked"),
            FetchError::Sensitive => write!(f, "content withheld (sensitive)"),
            FetchError::TooLarge => write!(f, "media too large"),
            FetchError::Transient(message) => write!(f, "transient: {message}"),
            FetchError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FetchError::Http(e) => Some(e),
            FetchError::Json(e) => Some(e),
            FetchError::Pixiv(e) => Some(e),
            FetchError::NotFound | FetchError::Blocked | FetchError::Sensitive => None,
            FetchError::TooLarge => None,
            FetchError::Transient(_) => None,
            FetchError::Io(e) => Some(e),
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
    let mut builder = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        // reqwest has no total timeout by default; a stalled connection
        // would otherwise pin a fetch/handler forever.
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10));
    // Route site fetches through the same proxy the Bot API uses, so a
    // network that needs TELOXIDE_PROXY (e.g. behind the GFW) does not
    // leave site fetches dead while the bot itself works.
    if let Some(proxy) = std::env::var("TELOXIDE_PROXY")
        .ok()
        .filter(|s| !s.is_empty())
        && let Ok(p) = reqwest::Proxy::all(&proxy)
    {
        builder = builder.proxy(p);
    }
    // Each `#[tokio::test]` runs on its own runtime; the connection pool is
    // bound to the runtime that created it, so cross-runtime reuse of idle
    // connections fails with DispatchGone. In test builds every request uses
    // a fresh connection. Production runs on one runtime and keeps pooling.
    #[cfg(test)]
    let builder = builder.pool_max_idle_per_host(0);
    builder.build().expect("failed to build HTTP client")
});

/// Whether a usable `ffmpeg` binary is on PATH (probed once). Shared by the
/// pixiv ugoira encoder and the bsky HLS remuxer.
static FFMPEG_AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
});

static FFMPEG_MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

pub(crate) fn ffmpeg_available() -> bool {
    *FFMPEG_AVAILABLE
}

pub(crate) fn log_once_ffmpeg_missing() {
    if !FFMPEG_MISSING_LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!("ffmpeg not found; ugoira and bsky video posts stay unsupported");
    }
}

/// Site adapter: one impl per supported site (twitter / bsky / pixiv),
/// registered in [`SITES`]. All site-specific knowledge — URL pattern,
/// cache-key format, fetch, retry policy, media-host headers, startup
/// validation — lives in the site module; the central dispatcher only
/// iterates the registry.
///
/// Async methods return a boxed future (see [`SiteFuture`]): `async fn` /
/// RPITIT in traits are not dyn-compatible (verified on rustc 1.95), and
/// `+ Send` is required since URL/queue workers spawn these futures. The
/// site structs are stateless unit structs, so the boxed futures never
/// borrow from `self` beyond the call's scope.
pub trait Site: Send + Sync {
    /// Stable site id (`"twitter"` / `"bsky"` / `"pixiv"`): caption-format
    /// lookup, cache-key prefixes and the SetFormat whitelist derive from it.
    fn id(&self) -> &'static str;
    /// URL pattern; the dispatcher's first match wins (dispatch order).
    fn pattern(&self) -> &'static Regex;
    /// Whether the site is usable (env token present, not disabled).
    fn enabled(&self) -> bool {
        true
    }
    /// Normalized cache key for a URL of this site (`None` when the URL does
    /// not match this site).
    fn cache_key(&self, url: &str) -> Option<String>;
    /// Fetches and normalizes a post.
    fn fetch_from_url<'a>(&'a self, url: &'a str) -> SiteFuture<'a, Fetched>;
    /// Retry policy for fetch errors: transient classes only.
    fn is_retryable(&self, err: &FetchError) -> bool {
        matches!(err, FetchError::Http(_) | FetchError::Transient(_))
    }
    /// Extra headers for downloading this site's media (hotlink protection,
    /// e.g. pixiv's Referer for pximg.net). Matched on the media URL, not
    /// the site pattern.
    fn media_headers(&self, _url: &str) -> Option<Vec<(&'static str, String)>> {
        None
    }
    /// Startup validation (token check etc.); failures are surfaced by
    /// [`validate_all`]. The default is a no-op.
    fn validate(&self) -> SiteFuture<'static, (), String> {
        Box::pin(async { Ok(()) })
    }
}

/// A boxed, `Send` future produced by a [`Site`] async method. Boxed so the
/// trait stays dyn-compatible; `Send` because URL/queue workers `tokio::spawn`
/// these futures.
type SiteFuture<'a, T, E = FetchError> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

/// The one registry of supported sites, in dispatch order (twitter → bsky →
/// pixiv). Adding a site = new module + one `Box::new(...)` entry here; the
/// bot crate never lists sites itself.
static SITES: LazyLock<Vec<Box<dyn Site>>> = LazyLock::new(|| {
    vec![
        Box::new(twitter::TwitterSite),
        Box::new(bsky::BskySite),
        Box::new(pixiv::PixivSite),
    ]
});

/// The first enabled site whose pattern matches `url`, in dispatch order.
fn find_site(url: &str) -> Option<&'static dyn Site> {
    SITES
        .iter()
        .find(|site| site.enabled() && site.pattern().is_match(url))
        .map(|site| site.as_ref())
}

/// Every supported site id, in dispatch order. The bot's SetFormat whitelist
/// derives from this list.
pub fn site_ids() -> Vec<&'static str> {
    SITES.iter().map(|site| site.id()).collect()
}

/// Runs every enabled site's startup validation and returns the failures
/// (site id + message). The caller logs / notifies; failing sites disable
/// themselves (pixiv disables on a bad token).
pub async fn validate_all() -> Vec<(&'static str, String)> {
    let mut failures = Vec::new();
    for site in SITES.iter() {
        if !site.enabled() {
            continue;
        }
        if let Err(e) = site.validate().await {
            failures.push((site.id(), e));
        }
    }
    failures
}

/// Fetches a post from its URL. Returns `Ok(None)` when no site pattern
/// matches (unsupported links are silently ignored by the bot).
///
/// Transient failures are retried: 3 total attempts with 1s then 2s delays.
/// What counts as transient is the matched site's own policy (`is_retryable`
/// — e.g. pixiv retries only network errors and 429/5xx). Permanent classes
/// (not-found, blocked, sensitive, parse failures, pixiv 4xx/auth errors)
/// are returned immediately; retrying them only wastes attempts against the
/// source site.
pub async fn fetch(url: &str) -> Result<Option<Fetched>, FetchError> {
    let Some(site) = find_site(url) else {
        return Ok(None);
    };
    for attempt in 0..3u32 {
        match site.fetch_from_url(url).await {
            Ok(fetched) => {
                // Per-request detail: debug only, keyed by the post id.
                log::debug!(
                    "fetched [key={}]: site {} returned {} media",
                    cache_key(url).unwrap_or_else(|| "?".into()),
                    fetched.site_name(),
                    fetched.media.len()
                );
                return Ok(Some(fetched));
            }
            Err(err) => {
                if site.is_retryable(&err) && attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                } else {
                    return Err(err);
                }
            }
        }
    }
    unreachable!("retry loop always returns")
}

/// Applies every site's media-header rule to a download request (pixiv's
/// `Referer` for pximg.net hotlink protection). Sites contribute via their
/// `media_headers(url)` — the central download code carries no per-site logic.
fn apply_media_headers(mut request: reqwest::RequestBuilder, url: &str) -> reqwest::RequestBuilder {
    for site in SITES.iter() {
        if let Some(headers) = site.media_headers(url) {
            for (name, value) in headers {
                request = request.header(name, value);
            }
        }
    }
    request
}

/// Downloads media bytes for the bot's upload fallback: when Telegram's own
/// fetch of a media URL is blocked (hotlink protection), the bot downloads
/// the file itself and uploads it via multipart. Site-appropriate headers
/// come from each site's `media_headers` (pixiv image hosts need `Referer`).
/// Returns the Content-Length of a media URL, or `None` when the server does
/// not report one. Used to check whether a file fits Telegram's size limits
/// before downloading/uploading it.
pub async fn media_size(url: &str) -> Result<Option<u64>, FetchError> {
    let response = apply_media_headers(CLIENT.get(url), url)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.content_length())
}

/// Downloads a media file with a hard size cap: the body is streamed and the
/// download aborts with [`FetchError::TooLarge`] the moment the cap is
/// crossed (or when a declared Content-Length already exceeds it). Keeps the
/// bot from buffering arbitrarily large bodies into memory.
pub async fn download_media_limited(url: &str, max_bytes: u64) -> Result<bytes::Bytes, FetchError> {
    let response = apply_media_headers(CLIENT.get(url), url)
        .send()
        .await?
        .error_for_status()?;
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(FetchError::TooLarge);
    }
    let mut response = response;
    let mut buf = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > max_bytes {
            return Err(FetchError::TooLarge);
        }
    }
    Ok(bytes::Bytes::from(buf))
}

pub async fn download_media(url: &str) -> Result<bytes::Bytes, FetchError> {
    download_media_limited(url, u64::MAX).await
}

/// Streams a download to `out`, aborting with [`FetchError::TooLarge`] the
/// moment the body crosses `max_bytes` (or when a declared Content-Length
/// already exceeds it). Unlike [`download_media_limited`] the body is never
/// buffered in memory — used for large files (e.g. the pixiv ugoira frame
/// zip, which can be hundreds of MB) that would otherwise spike RAM.
/// Returns the number of bytes written.
pub async fn download_media_to_file(
    url: &str,
    max_bytes: u64,
    out: &mut std::fs::File,
) -> Result<u64, FetchError> {
    use std::io::Write;
    let response = apply_media_headers(CLIENT.get(url), url)
        .send()
        .await?
        .error_for_status()?;
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(FetchError::TooLarge);
    }
    let mut response = response;
    let mut total: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(FetchError::TooLarge);
        }
        out.write_all(&chunk).map_err(FetchError::Io)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_normalizes_domain_variants() {
        assert_eq!(
            cache_key("https://x.com/user/status/1234567890/photo/1"),
            Some("twitter:1234567890".into())
        );
        assert_eq!(
            cache_key("https://mobile.twitter.com/user/status/1234567890"),
            Some("twitter:1234567890".into())
        );
        assert_eq!(
            cache_key("https://fxtwitter.com/user/status/1234567890"),
            Some("twitter:1234567890".into())
        );
        assert_eq!(
            cache_key("https://www.pixiv.net/artworks/123456"),
            Some("pixiv:123456".into())
        );
        assert_eq!(
            cache_key("https://bsky.app/profile/handle.example/post/3lorem"),
            Some("bsky:handle.example/3lorem".into())
        );
        assert_eq!(cache_key("https://example.com/not-a-post"), None);
    }

    #[test]
    fn site_id_from_key_parses_prefix() {
        assert_eq!(site_id_from_key("twitter:123"), "twitter");
        assert_eq!(site_id_from_key("pixiv:123"), "pixiv");
        assert_eq!(site_id_from_key("bsky:handle.example/3lorem"), "bsky");
        assert_eq!(site_id_from_key("unknown:1"), "unknown");
        assert_eq!(site_id_from_key("no-colon"), "unknown");
    }

    #[test]
    fn registry_lists_all_sites_in_dispatch_order() {
        assert_eq!(site_ids(), vec!["twitter", "bsky", "pixiv"]);
        // Enabled sites dispatch; unsupported URLs never match.
        assert!(find_site("https://x.com/u/status/1").is_some());
        assert!(find_site("https://bsky.app/profile/u/post/3x").is_some());
        assert!(find_site("https://example.com/x").is_none());
        // Cache keys are pattern-driven, independent of the enabled() gate
        // (pixiv is disabled in tests without PIXIV_REFRESH_TOKEN).
        assert_eq!(
            cache_key("https://www.pixiv.net/artworks/1"),
            Some("pixiv:1".into())
        );
    }

    #[test]
    fn caption_from_fields_substitutes_and_escapes() {
        // The format string is escaped, the field values are substituted
        // verbatim (callers pass the already-escaped render data).
        let out = caption_from_fields(
            "see {author} at {url} — {title}",
            "",
            "https://x.com/u/status/1",
            "A &amp; B",
            "https://x.com/u",
            "hello <world>",
            "",
        );
        assert_eq!(
            out,
            "see A &amp; B at https://x.com/u/status/1 — hello <world>"
        );
        // Empty format keeps the built-in caption untouched.
        assert_eq!(
            caption_from_fields("", "built-in", "u", "a", "au", "t", "g"),
            "built-in"
        );
    }

    #[test]
    fn truncate_caption_keeps_short_text() {
        assert_eq!(truncate_caption("short"), "short");
        // Exactly at the limit: untouched.
        let exact = "x".repeat(MAX_CAPTION_CHARS);
        assert_eq!(truncate_caption(&exact), exact);
    }

    #[test]
    fn truncate_caption_cuts_long_text_with_ellipsis() {
        let long = "x".repeat(MAX_CAPTION_CHARS + 100);
        let out = truncate_caption(&long);
        assert!(
            out.chars().count() <= MAX_CAPTION_CHARS,
            "len {}",
            out.chars().count()
        );
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_caption_does_not_split_an_html_entity() {
        // An entity crossing the cut must not be left half-open (&amp without ;).
        let mut long = "a".repeat(MAX_CAPTION_CHARS - 4);
        long.push_str("&amp;bbbb");
        let out = truncate_caption(&long);
        assert!(out.chars().count() <= MAX_CAPTION_CHARS);
        assert!(!out.contains("&amp"), "half entity left: {out:?}");
        assert!(!out.ends_with('&'));
    }

    #[test]
    fn truncate_caption_handles_multibyte_boundary() {
        // Multi-byte chars near the cut must not panic (char-boundary cut).
        let long = "界".repeat(MAX_CAPTION_CHARS + 10);
        let out = truncate_caption(&long);
        assert!(out.chars().count() <= MAX_CAPTION_CHARS);
    }

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
        // Empty-string check too: an unset CI secret arrives as "" (GitHub
        // Actions), which would otherwise run the test tokenless and fail.
        if std::env::var("PIXIV_REFRESH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .is_none()
        {
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
