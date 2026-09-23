//! The media-download stack: the two HTTP clients (site metadata vs. media,
//! which need different timeouts), the guard that keeps a download out of the
//! host's own network, and the two streaming entry points — a capped body in
//! memory ([`download_media_limited`]) and a large one written as it arrives
//! ([`download_media_to_file`]).
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
    // Redirects stay allowed (site CDNs use them), but every hop goes through
    // the same guard as the initial URL, and the cap stays reqwest's default:
    // a third-party response must not be able to walk the bot into the host's
    // own network.
    builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
        if !media_url_allowed(attempt.url()) {
            log::warn!("refusing a media redirect into the host's own network");
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
        Err(super::status_error("media", response.status()))
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

/// Whether an address must never be fetched. Media URLs come from a site's own
/// API response and the bytes are uploaded to Telegram, so following one into
/// the host's own network would turn the bot into a proxy for it: a cloud
/// metadata endpoint read back into a chat.
fn blocked_ip(addr: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()           // 10/8, 172.16/12, 192.168/16
                || v4.is_loopback()   // 127/8
                || v4.is_link_local() // 169.254/16 — the cloud metadata range
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                // Ranges the std helpers do not cover: carrier-grade NAT and
                // benchmarking.
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (18..=19).contains(&b))
        }
        IpAddr::V6(v6) => {
            let [first, ..] = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80 // link local fe80::/10
                || v6.to_ipv4_mapped().is_some_and(|v4| blocked_ip(IpAddr::V4(v4)))
        }
    }
}

/// `localhost` (and anything under it) plus the mDNS `.local` suffix: names that
/// only ever mean this machine.
fn is_local_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
}

/// Whether a media URL may be requested at all: http(s), and a host that is no
/// address or name of the host's own network. Applied to the URL a download
/// starts from *and* to every redirect hop.
///
/// The residual gap is DNS rebinding — a name the site controls that resolves to
/// a private address. Closing it needs a `reqwest::dns::Resolve` wrapper
/// filtering resolved addresses; it is deliberately not installed, because the
/// same resolver also resolves the operator's proxy host and `TELOXIDE_PROXY`
/// is routinely a LAN address, so the guard would take down a working
/// deployment to block a far less likely attack.
fn media_url_allowed(url: &url::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(v4)) => !blocked_ip(v4.into()),
        Some(url::Host::Ipv6(v6)) => !blocked_ip(v6.into()),
        Some(url::Host::Domain(name)) => !is_local_name(name),
        None => false,
    }
}

/// Prepares a media download: refuses a URL pointing inside the host's own
/// network ([`FetchError::Blocked`], permanent — the same URL would be refused
/// again), then applies the site's media headers. One choke point so every
/// download path gets the guard.
fn media_request(url: &str) -> Result<reqwest::RequestBuilder, FetchError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        log::warn!("media url is not a url: {e}");
        FetchError::Blocked
    })?;
    if !media_url_allowed(&parsed) {
        log::warn!("refusing to fetch media from the host's own network");
        return Err(FetchError::Blocked);
    }
    Ok(apply_media_headers(MEDIA_CLIENT.get(parsed), url))
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
    fn blocked_addresses_are_the_hosts_own_network() {
        for addr in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "0.0.0.0",
            "255.255.255.255",
            "100.64.0.1", // carrier-grade NAT
            "198.18.0.1", // benchmarking
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(blocked_ip(addr.parse().unwrap()), "{addr}");
        }
        for addr in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(!blocked_ip(addr.parse().unwrap()), "{addr}");
        }
    }

    #[test]
    fn media_urls_inside_the_host_are_refused() {
        for url in [
            "http://127.0.0.1:9/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:9/x",
            "https://localhost/",
            "https://prompt.localhost/x",
            "https://printer.local/x",
            "file:///etc/passwd",
            "gopher://example.com/1",
        ] {
            let parsed = url::Url::parse(url).unwrap();
            assert!(!media_url_allowed(&parsed), "{url}");
        }
        // Real media hosts and any public address stay fetchable.
        for url in [
            "https://i.pximg.net/img-original/img/1.jpg",
            "https://cdn.bsky.app/img/feed_thumbnail/plain/x",
            "http://example.com/a",
            "https://93.184.216.34/a",
        ] {
            let parsed = url::Url::parse(url).unwrap();
            assert!(media_url_allowed(&parsed), "{url}");
        }
    }
    /// The redirect-hop guard, against a public redirector: the initial URL is
    /// checked by [`media_request`], but a redirect is the part of the path a
    /// third-party response actually controls.

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to httpbin.org"]
    async fn live_redirect_into_the_hosts_network_is_refused() {
        let url = "https://httpbin.org/redirect-to?url=http://169.254.169.254/latest/meta-data/";
        match download_media_limited(url, u64::MAX, DOWNLOAD_TOTAL_TIMEOUT)
            .await
            .unwrap_err()
        {
            // A policy refusal reaches the caller wrapped by reqwest.
            FetchError::Http(e) => assert!(e.is_redirect(), "got {e}"),
            FetchError::Blocked => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_download_into_the_hosts_network_is_refused() {
        // Refused on the URL alone: nothing has to be listening (or leaking) at
        // the metadata endpoint for this to hold, and the class is permanent so
        // the send path does not retry it.
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9/secret",
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
