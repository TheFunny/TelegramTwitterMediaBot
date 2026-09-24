//! The media-download stack: the two HTTP clients (site metadata vs. media,
//! which need different timeouts), the CDN allowlist that keeps a download
//! out of the host's own network, and the two streaming entry points — a capped
//! body in memory ([`download_media_limited`]) and a large one written as it
//! arrives ([`download_media_to_file`]).
//!
//! Site-specific headers come from each adapter's `Site::media_headers`; no
//! code here knows about a particular site.

use super::{FetchError, SITES};
use std::sync::LazyLock;
use std::time::Duration;

/// How long a download may make no progress: the response head, and then each
/// individual chunk, must arrive within this window. Not a total timeout — see
/// [`DOWNLOAD_TOTAL_TIMEOUT`].
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute ceiling for one media download, on top of the idle window: a
/// server that drips a byte every 29 s keeps [`next_chunk`] satisfied
/// indefinitely, and a transfer that trickles forever holds whatever the
/// caller pinned to it — a fetch permit for an in-flight post, a prep slot
/// for the bot's upload fallback. Generous on purpose: the legitimate cases
/// are big — an ugoira frame zip runs to hundreds of MB and an HLS remux
/// pulls a whole video — so this is the budget for downloads *inside a
/// fetch*, while the slot-holding fallback passes its own shorter one (see
/// [`download_media_limited`]'s `total`). Checked between chunks, so a
/// transfer that completes just over the budget is kept rather than thrown
/// away.
pub(crate) const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

/// The error a download reports when it spends its whole budget without
/// finishing. Retryable: the transfer may simply have been unlucky, and a retry
/// of the post restarts the download.
fn download_too_slow(total: Duration) -> FetchError {
    FetchError::Transient(format!("download exceeded {}s", total.as_secs()))
}

/// Builds a client with the shared configuration (browser User-Agent, the
/// Bot API's proxy, per-runtime pools under test). `total_timeout` is what
/// differs between the two clients below.
fn build_client(total_timeout: Option<Duration>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .connect_timeout(Duration::from_secs(10));
    // Redirects stay allowed for allowlisted CDN hops, but every hop goes
    // through the same policy as the initial URL; a third-party response must
    // not be able to introduce a new host.
    builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
        if !media_url_allowed(attempt.url()) {
            log::warn!("refusing a media redirect outside the CDN allowlist");
            return attempt.error(FetchError::Blocked);
        }
        if attempt.previous().len() >= 10 {
            return attempt.stop();
        }
        attempt.follow()
    }));
    if let Some(total) = total_timeout {
        // reqwest has no total timeout by default; a stalled connection
        // would otherwise pin a fetch/handler forever.
        builder = builder.timeout(total);
    }
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
}

/// Shared HTTP client (browser User-Agent) for the site fetches — metadata
/// requests, where 30s is generous.
pub(crate) static CLIENT: LazyLock<reqwest::Client> =
    LazyLock::new(|| build_client(Some(Duration::from_secs(30))));

/// Client for media *downloads*, with no reqwest-level total timeout: a 10 MiB
/// fallback download, or an ugoira frame zip that may be hundreds of MB,
/// legitimately takes minutes on a slow link — a 30s total cap made those posts
/// impossible to deliver at all (the size cap said 512 MiB, the clock said 30s).
/// What a stalled connection cannot do is hang a worker: the head and every
/// chunk are bounded by [`DOWNLOAD_IDLE_TIMEOUT`] (see [`next_chunk`]), and a
/// transfer that keeps trickling but never finishes is bounded by the
/// caller's total budget (see [`download_media_limited`]).
static MEDIA_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| build_client(None));

/// The error a download reports when it stops making progress.
fn download_stalled() -> FetchError {
    FetchError::Transient(format!(
        "download stalled for {}s",
        DOWNLOAD_IDLE_TIMEOUT.as_secs()
    ))
}

/// Sends a media-download request: the response head must arrive within the
/// idle window, and a non-2xx status is classified by
/// [`super::status_error`] with `"media"` as the name — the same table the
/// site adapters use, so a dead URL and a bad moment read the same everywhere.
/// A transport error never reaches that table — it fails in `send()` and
/// stays [`FetchError::Http`].
async fn send_download(request: reqwest::RequestBuilder) -> Result<reqwest::Response, FetchError> {
    let response = match tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(download_stalled()),
    };
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(super::status_error("media", &response))
    }
}

/// One body chunk, or `None` at the end. A body that stops delivering is a
/// transient download error rather than a hang.
async fn next_chunk(response: &mut reqwest::Response) -> Result<Option<bytes::Bytes>, FetchError> {
    match tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, response.chunk()).await {
        Ok(Ok(chunk)) => Ok(chunk),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(download_stalled()),
    }
}

/// Reads a successful API response body with a hard byte cap.
pub(crate) async fn send_json_response(
    mut response: reqwest::Response,
    site: &'static str,
) -> Result<bytes::Bytes, FetchError> {
    if let Some(len) = response.content_length()
        && len > crate::site::MAX_SITE_JSON_BYTES as u64
    {
        return Err(FetchError::Site {
            site,
            error: "site response exceeds JSON size cap".into(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = next_chunk(&mut response).await? {
        if body.len().saturating_add(chunk.len()) > crate::site::MAX_SITE_JSON_BYTES {
            return Err(FetchError::Site {
                site,
                error: "site response exceeds JSON size cap".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(body))
}

/// `localhost` (and anything under it) plus the mDNS `.local` suffix.
fn is_local_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
}

/// Media is fetched only from the CDN families used by the site adapters.
/// IP literals are rejected as well: a public IP is not a member of that
/// allowlist, and accepting one would turn the bot into a generic proxy.
fn media_host_allowed(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "misskey.io"
        || name.ends_with(".misskey.io")
        || name == "misskeyusercontent.jp"
        || name.ends_with(".misskeyusercontent.jp")
        || name == "bsky.app"
        || name.ends_with(".bsky.app")
        || name == "twimg.com"
        || name.ends_with(".twimg.com")
        || name == "pximg.net"
        || name.ends_with(".pximg.net")
        || name == "hdslb.com"
        || name.ends_with(".hdslb.com")
}

fn media_url_allowed(url: &url::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    matches!(url.host(), Some(url::Host::Domain(name)) if !is_local_name(name) && media_host_allowed(name))
}

/// Prepares a media download: refuses a URL outside the CDN allowlist
/// ([`FetchError::Blocked`], permanent — the same URL would be refused again),
/// then applies every site's media-header rule (pixiv's `Referer` for pximg.net
/// hotlink protection; sites contribute via `media_headers(url)`, so the
/// central download code carries no other per-site logic). One choke point so
/// every download path gets both.
fn media_request(url: &str) -> Result<reqwest::RequestBuilder, FetchError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        log::warn!("media url is not a url: {e}");
        FetchError::Blocked
    })?;
    if !media_url_allowed(&parsed) {
        log::warn!("refusing media URL outside the CDN allowlist");
        return Err(FetchError::Blocked);
    }
    let mut request = MEDIA_CLIENT.get(parsed);
    for site in SITES.iter() {
        if let Some(headers) = site.media_headers(url) {
            for (name, value) in headers {
                request = request.header(name, value);
            }
        }
    }
    Ok(request)
}

/// Downloads a media file with a hard size cap: the body is streamed and the
/// download aborts with [`FetchError::TooLarge`] the moment the cap is
/// crossed (or when a declared Content-Length already exceeds it). Keeps the
/// bot from buffering arbitrarily large bodies into memory — the size check
/// the bot's upload fallback needs is the one here, not a probe of its own.
///
/// This is the bot's download path for the upload fallback: when Telegram
/// cannot fetch a media URL itself (hotlink protection), the bot downloads
/// the file and uploads it via multipart. Site-appropriate headers come from
/// each site's `media_headers` (pixiv image hosts need `Referer`).
///
/// `total` is this caller's whole-transfer budget. The bot's upload fallback
/// holds a prep slot (and its memory reservation) while this runs, so it
/// passes a shorter one of its own; bsky's in-fetch segments take the
/// generous [`super::DOWNLOAD_TOTAL_TIMEOUT`].
pub async fn download_media_limited(
    url: &str,
    max_bytes: u64,
    total: Duration,
) -> Result<bytes::Bytes, FetchError> {
    let response = send_download(media_request(url)?).await?;
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(FetchError::TooLarge);
    }
    let mut response = response;
    let mut buf = Vec::new();
    let started = std::time::Instant::now();
    while let Some(chunk) = next_chunk(&mut response).await? {
        if started.elapsed() > total {
            return Err(download_too_slow(total));
        }
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > max_bytes {
            return Err(FetchError::TooLarge);
        }
    }
    Ok(bytes::Bytes::from(buf))
}

/// Streams a download to `out`, aborting with [`FetchError::TooLarge`] the
/// moment the body crosses `max_bytes` (or when a declared Content-Length
/// already exceeds it). Unlike [`download_media_limited`] the body is never
/// buffered in memory — used for large files (e.g. the pixiv ugoira frame
/// zip, which can be hundreds of MB) that would otherwise spike RAM. Writes
/// go through the tokio handle so a sync write never stalls an executor
/// thread for the length of the download. Returns the number of bytes written.
pub async fn download_media_to_file(
    url: &str,
    max_bytes: u64,
    out: &mut tokio::fs::File,
) -> Result<u64, FetchError> {
    use tokio::io::AsyncWriteExt;
    let response = send_download(media_request(url)?).await?;
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(FetchError::TooLarge);
    }
    let mut response = response;
    let mut total: u64 = 0;
    let started = std::time::Instant::now();
    while let Some(chunk) = next_chunk(&mut response).await? {
        if started.elapsed() > DOWNLOAD_TOTAL_TIMEOUT {
            return Err(download_too_slow(DOWNLOAD_TOTAL_TIMEOUT));
        }
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(FetchError::TooLarge);
        }
        out.write_all(&chunk).await.map_err(FetchError::Io)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::{Fetched, pixiv};

    #[test]
    fn media_urls_outside_the_allowlist_are_refused() {
        for url in [
            "http://127.0.0.1:9/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:9/x",
            "https://1.1.1.1/x",
            "https://[2606:4700::1111]/x",
            "https://localhost/",
            "https://prompt.localhost/x",
            "https://printer.local/x",
            "https://example.com/a",
            "https://evil.pximg.net.attacker.example/a",
            "file:///etc/passwd",
            "gopher://example.com/1",
        ] {
            let parsed = url::Url::parse(url).unwrap();
            assert!(!media_url_allowed(&parsed), "{url}");
        }
        for url in [
            "https://i.pximg.net/img-original/img/1.jpg",
            "https://cdn.bsky.app/img/feed_thumbnail/plain/x",
            "https://pbs.twimg.com/media/1.jpg",
            "https://media.misskeyusercontent.jp/io/1.jpg",
            "https://i0.hdslb.com/bfs/1.jpg",
        ] {
            let parsed = url::Url::parse(url).unwrap();
            assert!(media_url_allowed(&parsed), "{url}");
        }
    }

    #[tokio::test]
    async fn a_download_from_a_refused_host_is_blocked() {
        // Refused on the URL alone: nothing has to be listening (or leaking) at
        // the metadata endpoint for this to hold, and the class is permanent so
        // the send path does not retry it.
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9/secret",
            "http://8.8.8.8/x",
        ] {
            let err = download_media_limited(url, u64::MAX, DOWNLOAD_TOTAL_TIMEOUT)
                .await
                .unwrap_err();
            assert!(matches!(err, FetchError::Blocked), "{url}: got {err:?}");
        }
        // A malformed URL is refused the same way instead of becoming a
        // retryable transport error.
        assert!(matches!(
            download_media_limited("not a url", u64::MAX, DOWNLOAD_TOTAL_TIMEOUT)
                .await
                .unwrap_err(),
            FetchError::Blocked
        ));
    }

    #[tokio::test]
    #[ignore = "live network: requires PIXIV_REFRESH_TOKEN and i.pximg.net"]
    async fn live_download_media_pixiv_original_with_referer() {
        // Proves the Referer header is attached for i.pximg.net: a header-less
        // GET to a pixiv original URL is rejected with 403. `#[ignore]` as
        // well as the token gate: this hit the CDN on every `cargo test
        // --workspace` in a token-exported shell (and flaked on a CDN body
        // timeout there), and the `live_` name puts it inside the CI live
        // job's `--ignored live` filter. Empty-string check too: an unset CI
        // secret arrives as "" (GitHub Actions), which would otherwise run
        // the test tokenless and fail — the `SKIP` prefix is what the live
        // job greps to tell a skip from a pass.
        if std::env::var("PIXIV_REFRESH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .is_none()
        {
            eprintln!("SKIP (no PIXIV_REFRESH_TOKEN): not running the pixiv download test");
            return;
        }
        let illustration = pixiv::fetch(126839080).await.unwrap();
        let fetched: Fetched = illustration.into();
        let url = match fetched.media.first() {
            Some(crate::media::Media::Illustration { url, .. }) => url.clone(),
            other => panic!("expected illustration media, got {other:?}"),
        };
        assert!(url.contains("i.pximg.net"));
        let bytes = download_media_limited(&url, u64::MAX, DOWNLOAD_TOTAL_TIMEOUT)
            .await
            .unwrap();
        assert!(!bytes.is_empty());
    }
}
