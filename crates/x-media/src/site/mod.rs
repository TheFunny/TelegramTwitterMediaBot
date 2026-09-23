//! Site fetching dispatcher and unified result types.
//!
//! Dispatch order: twitter → bsky → misskey → pixiv → bilibili. Each site
//! module exports a `PATTERN`, `enabled()` and `fetch_from_url()`; a future
//! site plugs in by adding one guarded entry in `SITES`.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;
use thiserror::Error;

pub mod bilibili;
pub mod bsky;
mod download;
pub mod misskey;
pub mod pixiv;
pub mod twitter;

pub use pixiv::PixivError;

pub(crate) use download::{CLIENT, DOWNLOAD_TOTAL_TIMEOUT};
pub use download::{download_media_limited, download_media_to_file};

/// The result of fetching a post: canonical URL, HTML caption, the post's
/// title and body, media list and spoiler flag. Produced by [`fetch`].
#[derive(Debug)]
pub struct Fetched {
    /// Canonical URL: `x.com/{author}/status/{id}` |
    /// `https://www.pixiv.net/artworks/{id}` |
    /// `https://bsky.app/profile/{handle}/post/{rkey}` |
    /// `https://www.bilibili.com/opus/{id}`
    pub source_url: String,
    /// The exact HTML produced by the site's caption().
    pub caption: String,
    /// The post's own title, where the platform has one: a pixiv artwork's
    /// title, the headline of a bilibili opus post or the title of the video
    /// an AV dynamic attaches. Empty on the platforms whose posts are text
    /// only (x/twitter, bsky, misskey) and on bilibili posts without a
    /// headline.
    pub title: String,
    /// The post's body text, as the platform exposes it: a tweet, a bsky or
    /// misskey post, a bilibili dynamic's text, a pixiv artwork's description
    /// (HTML flattened). Empty when the post has no text at all.
    pub content: String,
    pub media: Vec<crate::media::Media>,
    /// Spoiler flag for all media of this post.
    pub sensitive: bool,
    /// Site id (`"twitter"` / `"bsky"` / `"pixiv"` / `"bilibili"`): the single source of
    /// truth for site identity — caption-format lookup, cache-key prefix and
    /// the SetFormat whitelist all derive from it. Set by the producing site.
    pub site_id: &'static str,
    /// Raw values (pre-escaped) for user-customizable caption formats.
    pub(crate) render_data: Option<RenderData>,
    /// Keeps temp files (e.g. an encoded ugoira MP4) alive until the caller
    /// finishes uploading; not part of the public contract. Shared rather than
    /// owned because one fetched post can serve several sends — the bot shares
    /// one in-flight fetch between concurrent duplicates of the same link — and
    /// the files have to outlive every one of them.
    pub(crate) _keep_alive: Option<std::sync::Arc<tempfile::TempDir>>,
}

/// Values for the `{url} {author} {author_url} {title} {content} {tags}`
/// placeholders in user-supplied caption formats, substituted by
/// [`caption_from_fields`] as HTML text (never as an attribute value).
///
/// `author`, `title`, `content` and `tags` come from the site API (post
/// text, display names, descriptions) and are HTML-escaped at construction.
/// `url` and `author_url` stay raw: they are canonical URLs the adapter
/// builds from numeric ids and API-constrained handles/DIDs, so they carry
/// no escapable character — the bot's `/test` report relies on that when it
/// embeds them.
#[derive(Debug)]
pub(crate) struct RenderData {
    pub url: String,
    pub author: String,
    pub author_url: String,
    pub title: String,
    pub content: String,
    pub tags: String,
}

/// The built-in caption for a post that has no user-supplied format: the
/// canonical URL, the author as a link, then the post's text after a colon.
/// The two URLs are escaped for an HTML attribute and the text as HTML text,
/// so site-supplied content cannot inject markup. Shared by the adapters whose
/// captions have exactly this shape (bilibili, misskey).
pub fn caption(url: &str, author_url: &str, author: &str, text: &str) -> String {
    let url = html_escape::encode_double_quoted_attribute(url);
    let author_url = html_escape::encode_double_quoted_attribute(author_url);
    let author = html_escape::encode_text(author);
    if text.is_empty() {
        return format!("{url}\n<a href=\"{author_url}\">{author}</a>");
    }
    format!(
        "{url}\n<a href=\"{author_url}\">{author}</a>: {}",
        html_escape::encode_text(text)
    )
}

/// The post's text as one string: title and content joined by a line break,
/// each only when it is non-empty. This is what the sites' built-in captions
/// show after the author line, and what the bot quotes when it is long.
pub fn compose_text(title: &str, content: &str) -> String {
    match (title.is_empty(), content.is_empty()) {
        (false, false) => format!("{title}\n{content}"),
        (false, true) => title.to_string(),
        (true, false) => content.to_string(),
        (true, true) => String::new(),
    }
}

impl Fetched {
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
                &data.content,
                &data.tags,
            ),
            _ => truncate_caption(&self.caption),
        }
    }

    /// The pre-escaped placeholder values (author, author_url, title,
    /// content, tags) a caller needs to rebuild a caption later, e.g. for a
    /// cached post where the [`Fetched`] is no longer available.
    pub fn render_fields(&self) -> Option<(&str, &str, &str, &str, &str)> {
        self.render_data.as_ref().map(|d| {
            (
                d.author.as_str(),
                d.author_url.as_str(),
                d.title.as_str(),
                d.content.as_str(),
                d.tags.as_str(),
            )
        })
    }

    /// A reference to the temp dir keeping locally produced media (ugoira MP4,
    /// bsky remux MP4) alive. The bot keeps it while its task may still be
    /// retried by the queue, which runs after this [`Fetched`] is dropped and
    /// its temp files would otherwise be gone. `None` when no such dir exists;
    /// each clone keeps the directory alive for as long as it lives, so two
    /// sends of one post can each hold the same files.
    pub fn keep_alive(&self) -> Option<std::sync::Arc<tempfile::TempDir>> {
        self._keep_alive.clone()
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
///
/// One flat argument per placeholder keeps the two callers (the fresh and the
/// cached caption path) mirroring each other; the same shape as the bot's
/// `debug_report`.
#[allow(clippy::too_many_arguments)]
pub fn caption_from_fields(
    format: &str,
    built_in: &str,
    url: &str,
    author: &str,
    author_url: &str,
    title: &str,
    content: &str,
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
            .replace("{content}", content)
            .replace("{tags}", tags),
    )
}

/// Stable per-post cache key derived from any supported URL, so variant
/// domains (x.com / twitter.com / fxtwitter.com, mobile, `/photo/N`
/// suffixes) map to the same post. Delegates to each registered site's
/// `cache_key` (in registry order).
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

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("pixiv error: {0}")]
    Pixiv(#[from] PixivError),
    /// A site-specific error from a site that keeps its own error type.
    /// Permanent by default (sites that need retryable site errors convert
    /// them to [`FetchError::Http`] / [`FetchError::Transient`] before
    /// returning). Pixiv predates this and keeps the dedicated
    /// [`FetchError::Pixiv`] variant.
    #[error("{site} error: {error}")]
    Site {
        site: &'static str,
        #[source]
        error: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("not found")]
    NotFound,
    #[error("blocked")]
    Blocked,
    /// The URL matches a registered site that is disabled right now (pixiv
    /// without `PIXIV_REFRESH_TOKEN`, or after a failed login). Distinct from
    /// `Ok(None)` — an unsupported link — so the bot can tell the user why
    /// the link was not handled instead of silently ignoring it.
    #[error("{site} support is disabled")]
    Disabled { site: &'static str },
    /// The post exists but its content is withheld (twitter NSFW /
    /// age-restricted tweets come back as an empty `{}` from syndication).
    #[error("content withheld (sensitive)")]
    Sensitive,
    /// A download exceeded the caller's size cap (see [`download_media_limited`]).
    #[error("media too large")]
    TooLarge,
    /// The post was fetched, but its media could not be prepared locally — a
    /// download or encode step that runs *after* the site's own response
    /// (bsky's HLS remux, say). Deliberately not retryable: the retry would
    /// replay the whole fetch, redoing the download work that just failed
    /// instead of the request that failed.
    #[error("media could not be prepared: {0}")]
    MediaPrep(String),
    /// A transient server-side failure (429 / 5xx); [`fetch`] retries these.
    #[error("transient: {0}")]
    Transient(String),
    /// A local I/O failure while streaming a download to disk
    /// (see [`download_media_to_file`]).
    #[error("io error: {0}")]
    Io(std::io::Error),
}

/// The error class for a non-success HTTP status, shared by the site
/// adapters, the media downloads and twitter's auth fallback: 404/410 mean
/// the post is gone (permanent), any other client error the source answers
/// on sight is a refusal (permanent too — three retries only delay the same
/// answer), and only 408/429/5xx are a bad moment, retried by [`fetch`].
/// `site` only names the adapter in the message (`"media"` for downloads);
/// a site whose statuses mean something else (bilibili's 412 risk control,
/// misskey's 400 with `NO_SUCH_NOTE`) maps those before falling back here.
pub fn status_error(site: &'static str, status: reqwest::StatusCode) -> FetchError {
    match status.as_u16() {
        404 | 410 => FetchError::NotFound,
        code if status.is_client_error() && !matches!(code, 408 | 429) => FetchError::Blocked,
        _ => FetchError::Transient(format!("{site} status {status}")),
    }
}

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

/// Site adapter: one impl per supported site (twitter / bsky / misskey /
/// pixiv / bilibili), registered in `SITES`. All site-specific knowledge — URL pattern,
/// cache-key format, fetch, retry policy, media-host headers, startup
/// validation — lives in the site module; the central dispatcher only
/// iterates the registry.
///
/// Async methods return a boxed future (see `SiteFuture`): `async fn` /
/// RPITIT in traits are not dyn-compatible (verified on rustc 1.95), and
/// `+ Send` is required since URL/queue workers spawn these futures. The
/// site structs are stateless unit structs, so the boxed futures never
/// borrow from `self` beyond the call's scope.
pub trait Site: Send + Sync {
    /// Stable site id (`"twitter"` / `"bsky"` / `"misskey"` / `"pixiv"` /
    /// `"bilibili"`): caption-format lookup, cache-key prefixes and the
    /// SetFormat whitelist derive from it.
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
/// misskey → pixiv → bilibili). Adding a site = new module + one
/// `Box::new(...)` entry here; the bot crate never lists sites itself.
pub(crate) static SITES: LazyLock<Vec<Box<dyn Site>>> = LazyLock::new(|| {
    vec![
        Box::new(twitter::TwitterSite),
        Box::new(bsky::BskySite),
        Box::new(misskey::MisskeySite),
        Box::new(pixiv::PixivSite),
        Box::new(bilibili::BilibiliSite),
    ]
});

/// The first enabled site whose pattern matches `url`, in dispatch order.
fn find_site(url: &str) -> Option<&'static dyn Site> {
    SITES
        .iter()
        .find(|site| site.enabled() && site.pattern().is_match(url))
        .map(|site| site.as_ref())
}

/// The site whose pattern matches `url` but which is disabled right now.
/// `None` when no site matches the URL at all, or when the matching site is
/// enabled. Lets the dispatcher tell "unsupported link" (silently ignored)
/// apart from "this bot has that site switched off" (reported to the user).
fn disabled_site(url: &str) -> Option<&'static str> {
    SITES
        .iter()
        .find(|site| !site.enabled() && site.pattern().is_match(url))
        .map(|site| site.id())
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
/// matches (unsupported links are silently ignored by the bot) and
/// [`FetchError::Disabled`] when the URL belongs to a registered site that is
/// switched off right now — the two are different answers for the user.
///
/// Transient failures are retried: 3 total attempts with 1s then 2s delays.
/// What counts as transient is the matched site's own policy (`is_retryable`
/// — e.g. pixiv retries only network errors and 429/5xx). Permanent classes
/// (not-found, blocked, sensitive, parse failures, pixiv 4xx/auth errors)
/// are returned immediately; retrying them only wastes attempts against the
/// source site.
pub async fn fetch(url: &str) -> Result<Option<Fetched>, FetchError> {
    fetch_with_attempts(url, MAX_FETCH_ATTEMPTS).await
}

/// [`fetch`] without the retry backoff (one attempt). For callers with a
/// short deadline: an inline query's answer window is measured in seconds, so
/// the 1s + 2s retry sleeps would outlast the query the answer belongs to.
pub async fn fetch_once(url: &str) -> Result<Option<Fetched>, FetchError> {
    fetch_with_attempts(url, 1).await
}

/// Total attempts of the retried [`fetch`] (3: the initial try plus two).
const MAX_FETCH_ATTEMPTS: u32 = 3;

async fn fetch_with_attempts(url: &str, attempts: u32) -> Result<Option<Fetched>, FetchError> {
    // Wall time of the whole fetch, retry backoff included: the ugoira encode
    // and the HLS remux live inside it, so this is where a slow fetch shows.
    let started = std::time::Instant::now();
    let Some(site) = find_site(url) else {
        // A registered-but-disabled site (pixiv without a token) is not an
        // unsupported link: report it, so the bot answers the user instead of
        // ignoring the message.
        return match disabled_site(url) {
            Some(site) => Err(FetchError::Disabled { site }),
            None => Ok(None),
        };
    };
    for attempt in 0..attempts.max(1) {
        match site.fetch_from_url(url).await {
            Ok(fetched) => {
                // Per-request detail: debug only, keyed by the post id.
                log::debug!(
                    "fetched [key={}]: site {} returned {} media in {}ms",
                    cache_key(url).unwrap_or_else(|| "?".into()),
                    fetched.site_id,
                    fetched.media.len(),
                    started.elapsed().as_millis()
                );
                return Ok(Some(fetched));
            }
            Err(err) => {
                if site.is_retryable(&err) && attempt + 1 < attempts {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                } else {
                    return Err(err);
                }
            }
        }
    }
    unreachable!("retry loop always returns")
}

/// Whether fetching `url` requires site-specific headers (pixiv's `Referer`
/// for `pximg.net` hotlink protection, see [`Site::media_headers`]). Telegram's
/// own fetch of a media URL sends none of them, so a URL that needs them fails
/// there — callers that hand a URL to Telegram (inline query results) must
/// skip such media instead of shipping a broken item.
pub fn needs_media_headers(url: &str) -> bool {
    SITES.iter().any(|site| site.media_headers(url).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locally produced media (ugoira MP4, bsky remux MP4) lives in a temp dir
    /// whose lifetime is refcounted: one fetch result can serve several sends
    /// (the bot shares one in-flight fetch between concurrent duplicates), and
    /// the files must outlive all of them — but no longer than the last one.
    #[test]
    fn a_keep_alive_clone_outlives_the_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.mp4");
        std::fs::write(&file, b"mp4").unwrap();
        let fetched = Fetched {
            source_url: "https://x.com/u/status/1".into(),
            caption: String::new(),
            title: String::new(),
            content: String::new(),
            media: Vec::new(),
            sensitive: false,
            site_id: "twitter",
            render_data: None,
            _keep_alive: Some(std::sync::Arc::new(dir)),
        };

        let shared = fetched.keep_alive().expect("a temp dir to share");
        drop(fetched);
        assert!(file.exists(), "the file must survive the fetched post");

        let second = shared.clone();
        drop(shared);
        assert!(file.exists(), "another holder keeps it alive");
        drop(second);
        assert!(!file.exists(), "the last holder releases the directory");
    }

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
        assert_eq!(
            cache_key("https://t.bilibili.com/1245284537985925159"),
            Some("bilibili:1245284537985925159".into())
        );
        assert_eq!(cache_key("https://example.com/not-a-post"), None);
    }

    #[test]
    fn site_id_from_key_parses_prefix() {
        assert_eq!(site_id_from_key("twitter:123"), "twitter");
        assert_eq!(site_id_from_key("pixiv:123"), "pixiv");
        assert_eq!(site_id_from_key("bsky:handle.example/3lorem"), "bsky");
        assert_eq!(site_id_from_key("bilibili:123"), "bilibili");
        assert_eq!(site_id_from_key("unknown:1"), "unknown");
        assert_eq!(site_id_from_key("no-colon"), "unknown");
    }

    #[test]
    fn registry_lists_all_sites_in_dispatch_order() {
        assert_eq!(
            site_ids(),
            vec!["twitter", "bsky", "misskey", "pixiv", "bilibili"]
        );
        // Enabled sites dispatch; unsupported URLs never match.
        assert!(find_site("https://x.com/u/status/1").is_some());
        assert!(find_site("https://misskey.io/notes/abc").is_some());
        assert!(find_site("https://t.bilibili.com/1245284537985925159").is_some());
        assert!(find_site("https://example.com/x").is_none());
        // Cache keys are pattern-driven, independent of the enabled() gate
        // (pixiv is disabled in tests without PIXIV_REFRESH_TOKEN).
        assert_eq!(
            cache_key("https://www.pixiv.net/artworks/1"),
            Some("pixiv:1".into())
        );
    }

    #[test]
    fn site_error_variant_displays_and_sources() {
        use std::error::Error as _;
        let err = FetchError::Site {
            site: "example",
            error: Box::new(std::io::Error::other("boom")),
        };
        assert_eq!(err.to_string(), "example error: boom");
        assert!(err.source().is_some());
        // Permanent by default: no site's is_retryable matches it (the trait
        // default is the policy for every site that does not override it).
        assert!(!twitter::TwitterSite.is_retryable(&err));
        assert!(twitter::TwitterSite.is_retryable(&FetchError::Transient("429".into())));
    }

    #[test]
    fn caption_from_fields_substitutes_and_escapes() {
        // The format string is escaped, the field values are substituted
        // verbatim (callers pass the already-escaped render data).
        let out = caption_from_fields(
            "see {author} at {url} — {title}: {content}",
            "",
            "https://x.com/u/status/1",
            "A &amp; B",
            "https://x.com/u",
            "hello <world>",
            "the body",
            "",
        );
        assert_eq!(
            out,
            "see A &amp; B at https://x.com/u/status/1 — hello <world>: the body"
        );
        // Empty format keeps the built-in caption untouched.
        assert_eq!(
            caption_from_fields("", "built-in", "u", "a", "au", "t", "c", "g"),
            "built-in"
        );
    }

    #[test]
    fn compose_text_joins_title_and_content() {
        assert_eq!(compose_text("标题", "正文"), "标题\n正文");
        assert_eq!(compose_text("标题", ""), "标题");
        assert_eq!(compose_text("", "正文"), "正文");
        assert_eq!(compose_text("", ""), "");
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
        // The exact output is what pins the guard: a cut that keeps `&am` (no
        // `;`) leaves a half-open entity that `!contains("&amp")` cannot see,
        // so the old assertions stayed green with the guard deleted. Both
        // directions matter — an entity the cut falls inside is dropped whole,
        // one the cut falls after is kept whole.
        for (long, expected) in [
            (
                "a".repeat(MAX_CAPTION_CHARS - 4) + "&amp;bbbb",
                "a".repeat(MAX_CAPTION_CHARS - 4) + "…",
            ),
            (
                "a".repeat(MAX_CAPTION_CHARS - 6) + "&amp;bbbb",
                "a".repeat(MAX_CAPTION_CHARS - 6) + "&amp;…",
            ),
        ] {
            let out = truncate_caption(&long);
            assert_eq!(out, expected);
            assert!(out.chars().count() <= MAX_CAPTION_CHARS, "{out:?}");
        }
    }

    #[test]
    fn truncate_caption_handles_multibyte_boundary() {
        // Multi-byte chars near the cut must not panic (char-boundary cut).
        let long = "界".repeat(MAX_CAPTION_CHARS + 10);
        let out = truncate_caption(&long);
        assert!(out.chars().count() <= MAX_CAPTION_CHARS);
    }

    #[tokio::test]
    async fn unsupported_urls_return_none() {
        // Neither a URL no site pattern matches nor a string that is no URL at
        // all is an error: both answer `Ok(None)`, which is what keeps the bot
        // silent on links it cannot handle (only a registered-but-disabled site
        // gets a reply).
        for url in ["https://example.com/some/article", "not a url at all"] {
            let result = fetch(url).await;
            assert!(matches!(result, Ok(None)), "{url}: got {result:?}");
        }
    }

    #[test]
    fn media_headers_are_reported_only_where_telegram_would_fail() {
        // pixiv's CDN needs a Referer, which only the bot can send: an inline
        // result pointing at it renders broken, so callers skip it.
        assert!(needs_media_headers(
            "https://i.pximg.net/img-original/img/2024/01/01/00/00/00/1_p0.jpg"
        ));
        // The rest serve direct requests (verified per site in their modules).
        for url in [
            "https://pbs.twimg.com/media/1.jpg",
            "https://cdn.bsky.app/img/1.jpg",
            "https://media.misskeyusercontent.jp/io/1.webp",
            "https://i0.hdslb.com/bfs/1.jpg",
        ] {
            assert!(!needs_media_headers(url), "{url}");
        }
    }

    #[test]
    fn persistent_client_statuses_are_permanent() {
        use reqwest::StatusCode;
        // The one table every caller shares now: only 408, 429 and 5xx can
        // answer differently on a retry. A 400 used to be Transient here and
        // in two local fallbacks — twitter syndication's broken-token 400, for
        // one, burned three retries per link before saying the same thing.
        assert!(matches!(
            status_error("x", StatusCode::NOT_FOUND),
            FetchError::NotFound
        ));
        assert!(matches!(
            status_error("x", StatusCode::BAD_REQUEST),
            FetchError::Blocked
        ));
        assert!(matches!(
            status_error("x", StatusCode::PAYLOAD_TOO_LARGE),
            FetchError::Blocked
        ));
        assert!(matches!(
            status_error("x", StatusCode::REQUEST_TIMEOUT),
            FetchError::Transient(_)
        ));
        assert!(matches!(
            status_error("x", StatusCode::TOO_MANY_REQUESTS),
            FetchError::Transient(_)
        ));
        assert!(matches!(
            status_error("x", StatusCode::INTERNAL_SERVER_ERROR),
            FetchError::Transient(_)
        ));
        // The download path delegates under its own name, same classes.
        assert!(matches!(
            status_error("media", StatusCode::BAD_REQUEST),
            FetchError::Blocked
        ));
    }

    #[tokio::test]
    async fn disabled_site_is_reported_not_ignored() {
        // pixiv is the only token-gated site; with PIXIV_REFRESH_TOKEN set it
        // is enabled and this link would hit the network, so skip then.
        if std::env::var("PIXIV_REFRESH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            eprintln!("skipping: PIXIV_REFRESH_TOKEN is set");
            return;
        }
        let result = fetch("https://www.pixiv.net/artworks/1").await;
        assert!(
            matches!(result, Err(FetchError::Disabled { site: "pixiv" })),
            "got {result:?}"
        );
        // The cache key still resolves: the bot keys the reply and the link
        // cache off it even when the site is off.
        assert_eq!(
            cache_key("https://www.pixiv.net/artworks/1"),
            Some("pixiv:1".into())
        );
    }
}
