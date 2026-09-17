//! Site adapter for Bilibili dynamics (图片 / 动图): URL pattern, API fetch and
//! normalization into [`Fetched`] (see [`crate::site::Site`]).
//!
//! Scope: the *media images* of a dynamic — the `major.draw` image grid and
//! the cover of an attached video. Animated (`.gif`) pictures become
//! [`Media::Animated`], everything else a photo. The video stream itself is
//! deliberately **not** resolved: `dyn_archive` carries no `cid`, so playing
//! it would mean a second `x/web-interface/view` round trip plus
//! `x/player/playurl` and its size/quality chasing — the cover plus the post
//! link is what the operator asked for.
//!
//! Text: an image/text post is read from its opus serialization
//! (`features=itemOpusStyle` — the legacy serialization carries no text at
//! all), a forward from its own body plus the quote's, and a 视频投稿动态
//! from the archive card's title, which it has instead of a body.
//!
//! `b23.tv` short links are not matched: most of them point at videos, which
//! this adapter does not handle, and matching them would turn a silently
//! ignored link into the bot's "Failed to fetch media" reply.
//!
//! Verified live (2026-09-17): the detail endpoint answers **anonymously**
//! (no cookie, no WBI signature) to a browser-ish `User-Agent` + `Referer`;
//! `build`-taking variants and bilibili's own `bilibili_pc/…` UA got `-352`.
//! Risk control escalates with request volume from one IP — first a plain
//! request starts answering `-352`, then adding the anonymous device cookies
//! (`buvid3` + `buvid4`, fetched from [`SPI_URL`] and sent by [`cookie`])
//! restores `code: 0`, and a heavily flagged IP stays blocked until
//! `BILIBILI_COOKIE` supplies a logged-in session.

use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched, RenderData, Site, SiteFuture, compose_text};
use html_escape::{encode_double_quoted_attribute, encode_text};
use regex::Regex;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

const API_URL: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1/detail";

/// Anonymous device-fingerprint endpoint handing out `buvid3`/`buvid4`.
const SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";

/// Sent with every API request: requests without it are the ones bilibili
/// risk-controls (`code -352`, HTTP 412).
const REFERER: &str = "https://www.bilibili.com/";

/// Web feature flag that makes the detail endpoint serialize an image/text
/// post as `major.opus` instead of the legacy `major.draw` + `desc` pair.
///
/// Not optional: without it an opus post comes back with **no text at all**
/// (`desc: null` and no `major.opus`), which is how the document title and
/// body — the post's actual content — used to be lost (verified 2026-09-17
/// on `opus/1248857553488576532`: legacy `desc: null`, flagged
/// `major.opus.summary.text = "[doge_金箍]黑白搭配"`). The adapter still
/// parses the legacy shape as a fallback, in case the flag is retired.
const OPUS_FEATURE: &str = "itemOpusStyle";

/// hdslb image variant used as thumbnail (and as the oversized fallback): a
/// downscaled still of the same image, ~30 KB instead of ~570 KB. Verified
/// live for `.jpg` and `.webp` sources. Not applied to `.gif` (unverified for
/// animated sources, and Telegram generates its own frame preview).
const THUMB_SUFFIX: &str = "@518w.jpg";

/// Optional `Cookie` header value (`SESSDATA=…; bili_jct=…`) for deployments
/// that need a logged-in session. Unset by default: the adapter fetches
/// bilibili's anonymous device cookies itself (see [`cookie`]) and works
/// without any operator setup, so unlike pixiv the site stays enabled.
static COOKIE: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("BILIBILI_COOKIE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
});

/// Cached `buvid3`/`buvid4` header value from [`SPI_URL`], or `None` when the
/// fingerprint endpoint was unavailable (requests then go out without a
/// cookie, as before).
static BUVID: LazyLock<tokio::sync::Mutex<Option<String>>> = LazyLock::new(Default::default);

/// Registry entry for the bilibili adapter (see [`crate::site::Site`]).
pub struct BilibiliSite;

impl Site for BilibiliSite {
    fn id(&self) -> &'static str {
        "bilibili"
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

/// Every supported link form, with the dynamic id in group 1: the direct
/// dynamic (web / mobile / h5 share) and opus URLs.
pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:https?://)?(?:www|t|m)\.bilibili\.com/(?:opus/|dynamic/|h5/dynamic/detail/)?(\d+)",
    )
    .unwrap()
});

pub fn enabled() -> bool {
    true
}

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let dynamic_id = PATTERN
        .captures(url)
        .and_then(|caps| caps.get(1))
        .ok_or(FetchError::NotFound)?
        .as_str();
    let item = fetch(dynamic_id).await?;
    Ok(item.into())
}

/// Cache key for a bilibili URL: `"bilibili:<dynamic id>"`. The prefix is the
/// site id used for caption-format lookup and link-cache keys.
pub fn cache_key(url: &str) -> Option<String> {
    PATTERN
        .captures(url)
        .map(|caps| format!("bilibili:{}", &caps[1]))
}

/// Bilibili's fetch-retry policy: transient classes only. Not-found, blocked
/// and parse failures are permanent.
pub fn is_retryable(err: &FetchError) -> bool {
    matches!(err, FetchError::Http(_) | FetchError::Transient(_))
}

/// hdslb media serves without a `Referer` (verified live 2026-09-17 on
/// `i0.hdslb.com` image URLs, requested both with and without one), so no
/// extra headers.
pub fn media_headers(_url: &str) -> Option<Vec<(&'static str, String)>> {
    None
}

/// `Cookie` header for bilibili requests: the operator's `BILIBILI_COOKIE`
/// when set, otherwise the anonymous device cookies.
///
/// Device cookies are the adapter's own fix for risk control, not merely
/// insurance — verified 2026-09-17 on an IP bilibili had flagged: every
/// request answered `-352` without them and `code: 0` with `buvid3` +
/// `buvid4`. A logged-in `BILIBILI_COOKIE` carries its own device cookies,
/// hence precedence rather than concatenation.
async fn cookie() -> Option<String> {
    if let Some(cookie) = COOKIE.as_deref() {
        return Some(cookie.to_string());
    }
    // ponytail: cached for the process lifetime. Refetching after a `-352`
    // would mint a new device id for the same flagged IP — the escalation
    // path is BILIBILI_COOKIE.
    let mut cached = BUVID.lock().await;
    if cached.is_none() {
        *cached = match fetch_buvid().await {
            Ok(cookie) => cookie,
            Err(e) => {
                log::debug!("bilibili fingerprint unavailable: {e}");
                None
            }
        };
    }
    cached.clone()
}

/// Fetches the device cookies bilibili hands to any visitor. The result is
/// deliberately not an error: an unavailable fingerprint endpoint just means
/// requests go out without a cookie.
async fn fetch_buvid() -> Result<Option<String>, FetchError> {
    let response = crate::site::CLIENT.get(SPI_URL).send().await?;
    let fingerprint: model::Fingerprint = response.json().await.map_err(|e| FetchError::Site {
        site: "bilibili",
        error: Box::new(e),
    })?;
    Ok(buvid_cookie(&fingerprint))
}

/// `buvid3=B; buvid4=B` from a fingerprint response; `None` when it carried
/// no device ids.
fn buvid_cookie(fingerprint: &model::Fingerprint) -> Option<String> {
    let data = fingerprint.data.as_ref()?;
    if data.buvid3.is_empty() && data.buvid4.is_empty() {
        return None;
    }
    Some(format!("buvid3={}; buvid4={}", data.buvid3, data.buvid4))
}

async fn request(url: &str) -> reqwest::RequestBuilder {
    let request = crate::site::CLIENT.get(url).header("Referer", REFERER);
    match cookie().await {
        Some(cookie) => request.header("Cookie", cookie),
        None => request,
    }
}

/// Fetches one dynamic by id and returns its item.
///
/// The API answers client-level failures with HTTP 200 + a business `code`
/// (see [`code_error`]); HTTP 412 is bilibili's risk-control page.
pub async fn fetch(dynamic_id: &str) -> Result<model::Item, FetchError> {
    let response = request(API_URL)
        .await
        .query(&[("id", dynamic_id), ("features", OPUS_FEATURE)])
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            412 => risk_control("412"),
            _ => FetchError::Transient(format!("bilibili status {status}")),
        });
    }
    let detail: model::Detail = response.json().await.map_err(|e| FetchError::Site {
        site: "bilibili",
        error: Box::new(e),
    })?;
    if let Some(err) = code_error(detail.code, detail.message.as_deref().unwrap_or_default()) {
        return Err(err);
    }
    detail
        .data
        .and_then(|data| data.item)
        .map(|item| *item)
        .ok_or(FetchError::NotFound)
}

/// Maps the API's business code to an error; `None` means success.
///
/// `-352`/`-412` are bilibili's risk control and are mapped to a *transient*
/// error on purpose: the queue retries them with backoff instead of dropping
/// the post. A removed or nonexistent dynamic answers `500` (verified
/// 2026-09-17; the older `4101147` is kept because nazurin still documents
/// it) — permanent, so a dead link is not retried. Codes that were never
/// observed on a live post (e.g. `4101105 请求数据发生错误`, which only ever
/// came back for ids that cannot exist) stay on the permanent arm too: its
/// message hints at a retry, but the user-visible outcome is the same.
fn code_error(code: i64, message: &str) -> Option<FetchError> {
    match code {
        0 => None,
        -352 | -412 => Some(risk_control(&code.to_string())),
        500 | 4101147 => Some(FetchError::NotFound),
        _ => Some(FetchError::Site {
            site: "bilibili",
            error: format!("code {code}: {message}").into(),
        }),
    }
}

/// Risk control: retryable, but retrying rarely helps on its own — the
/// operator's lever is `BILIBILI_COOKIE`, hence the hint. Logged once so a
/// blocked deployment does not flood the log with one line per link.
fn risk_control(code: &str) -> FetchError {
    if !RISK_CONTROL_LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!("bilibili risk control ({code}); set BILIBILI_COOKIE if this persists");
    }
    FetchError::Transient(format!("bilibili risk control ({code})"))
}

static RISK_CONTROL_LOGGED: AtomicBool = AtomicBool::new(false);

impl From<model::Item> for Fetched {
    fn from(item: model::Item) -> Self {
        let url = format!("https://www.bilibili.com/opus/{}", item.id_str);
        let author = author_name(&item).to_string();
        let author_url = author_url(&item).unwrap_or_else(|| url.clone());
        let (title, content) = text_parts(&item);
        let text = compose_text(&title, &content);
        let tags = topic_name(&item).to_string();

        let caption = caption(&url, &author_url, &author, &text);
        let media = media_of(&item);

        Fetched {
            source_url: url.clone(),
            caption,
            title: title.clone(),
            content: content.clone(),
            media,
            sensitive: false,
            site_id: "bilibili",
            render_data: Some(RenderData {
                url,
                author: encode_text(&author).into_owned(),
                author_url,
                title: encode_text(&title).into_owned(),
                content: encode_text(&content).into_owned(),
                tags: encode_text(&tags).into_owned(),
            }),
            _keep_alive: None,
        }
    }
}

fn author_name(item: &model::Item) -> &str {
    item.modules
        .as_ref()
        .and_then(|modules| modules.module_author.as_ref())
        .map(|author| author.name.as_str())
        .unwrap_or_default()
}

fn author_url(item: &model::Item) -> Option<String> {
    item.modules
        .as_ref()
        .and_then(|modules| modules.module_author.as_ref())
        .and_then(|author| author.mid)
        .map(|mid| format!("https://space.bilibili.com/{mid}"))
}

fn desc_text(item: &model::Item) -> &str {
    item.modules
        .as_ref()
        .and_then(|modules| modules.module_dynamic.as_ref())
        .and_then(|dynamic| dynamic.desc.as_ref())
        .map(|desc| desc.text.as_str())
        .unwrap_or_default()
}

fn major(item: &model::Item) -> Option<&model::Major> {
    item.modules
        .as_ref()
        .and_then(|modules| modules.module_dynamic.as_ref())
        .and_then(|dynamic| dynamic.major.as_ref())
}

fn opus(item: &model::Item) -> Option<&model::Opus> {
    major(item).and_then(|major| major.opus.as_ref())
}

fn archive_title(item: &model::Item) -> Option<&str> {
    major(item)
        .and_then(|major| major.archive.as_ref())
        .and_then(|archive| archive.title.as_deref())
        .filter(|title| !title.trim().is_empty())
}

/// The dynamic's own words, richest source first: the opus document's
/// headline and body → the legacy body (`module_dynamic.desc.text`) → the
/// attached video's card title, which is the text of a 视频投稿动态 because
/// such a post has no body anywhere.
///
/// The opus shape is what makes ordinary 图文 posts readable at all — their
/// legacy serialization has no text.
fn own_parts(item: &model::Item) -> (String, String) {
    if let Some(opus) = opus(item) {
        let title = opus.title.as_deref().unwrap_or_default().trim().to_string();
        let body = opus
            .summary
            .as_ref()
            .map(|summary| summary.text.trim().to_string())
            .unwrap_or_default();
        if !title.is_empty() || !body.is_empty() {
            return (title, body);
        }
    }
    let body = desc_text(item).trim();
    if !body.is_empty() {
        return (String::new(), body.to_string());
    }
    (
        archive_title(item).unwrap_or_default().to_string(),
        String::new(),
    )
}

fn topic_name(item: &model::Item) -> &str {
    item.modules
        .as_ref()
        .and_then(|modules| modules.module_dynamic.as_ref())
        .and_then(|dynamic| dynamic.topic.as_ref())
        .map(|topic| topic.name.as_str())
        .unwrap_or_default()
}

/// The post's title and body: the dynamic's own, with the quoted original's
/// text appended to the body the way bilibili's web UI shows forwards
/// (`//@author:`).
fn text_parts(item: &model::Item) -> (String, String) {
    let (title, mut content) = own_parts(item);
    let Some(orig) = item.orig.as_deref() else {
        return (title, content);
    };
    let (orig_title, orig_content) = own_parts(orig);
    let orig_text = compose_text(&orig_title, &orig_content);
    if orig_text.is_empty() {
        return (title, content);
    }
    let name = author_name(orig);
    if !content.is_empty() {
        content.push('\n');
    }
    if name.is_empty() {
        content.push_str(&orig_text);
    } else {
        content.push_str(&format!("//@{name}:\n{orig_text}"));
    }
    (title, content)
}

/// The dynamic's media: its own grid (or video cover), falling back to the
/// quoted original's when a forward shell has none (mirrors misskey's
/// `effective()` handling of renotes).
fn media_of(item: &model::Item) -> Vec<Media> {
    let own = own_media(item);
    if !own.is_empty() {
        return own;
    }
    item.orig.as_deref().map(own_media).unwrap_or_default()
}

fn own_media(item: &model::Item) -> Vec<Media> {
    // Flagged serialization first: image posts put their pictures in
    // `major.opus.pics` (and carry no `major.draw` at all).
    if let Some(opus) = opus(item) {
        let pics: Vec<Media> = opus
            .pics
            .iter()
            .filter_map(|pic| pic.url().and_then(image))
            .collect();
        if !pics.is_empty() {
            return pics;
        }
    }
    let Some(major) = major(item) else {
        return Vec::new();
    };
    if let Some(draw) = major.draw.as_ref() {
        return draw
            .items
            .iter()
            .filter_map(|pic| pic.url().and_then(image))
            .collect();
    }
    major
        .archive
        .as_ref()
        .and_then(|archive| archive.cover.as_deref())
        .and_then(image)
        .into_iter()
        .collect()
}

/// Maps one image URL: bilibili serves it as `http://`, and a `.gif` source
/// is an animation rather than a photo.
fn image(url: &str) -> Option<Media> {
    let url = to_https(url);
    if !url.starts_with("https://") {
        return None;
    }
    Some(if url.ends_with(".gif") {
        Media::Animated {
            title: None,
            url,
            // Left empty on purpose: the `@518w.jpg` variant is unverified for
            // animated sources, and Telegram generates a frame preview itself.
            thumbnail_url: String::new(),
        }
    } else {
        Media::Illustration {
            title: None,
            // Written before `url` moves so the formatting borrows it.
            thumbnail_url: Some(format!("{url}{THUMB_SUFFIX}")),
            url,
            fallback_url: None,
        }
    })
}

/// Bilibili serves media as `http://` (and sometimes protocol-relative
/// `//host/…`); Telegram only accepts `https://`.
fn to_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

fn caption(url: &str, author_url: &str, author: &str, text: &str) -> String {
    let url = encode_double_quoted_attribute(url);
    let author_url = encode_double_quoted_attribute(author_url);
    let author = encode_text(author);
    if text.is_empty() {
        return format!("{url}\n<a href=\"{author_url}\">{author}</a>");
    }
    format!(
        "{url}\n<a href=\"{author_url}\">{author}</a>: {}",
        encode_text(text)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_json(major: serde_json::Value, text: &str) -> serde_json::Value {
        serde_json::json!({
            "id_str": "1245284537985925159",
            "modules": {
                "module_author": { "name": "索尼音乐中国", "mid": 486906719 },
                "module_dynamic": {
                    "desc": { "text": text },
                    "major": major,
                    "topic": { "id": 1347638, "name": "音乐" },
                },
            },
        })
    }

    fn parse(json: serde_json::Value) -> Fetched {
        serde_json::from_value::<model::Item>(json).unwrap().into()
    }

    fn draw_item(src: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "MAJOR_TYPE_DRAW",
            "draw": { "items": [{ "src": src, "width": 2304, "height": 2880, "size": 566.7 }] },
        })
    }

    #[test]
    fn pattern_matches_supported_forms() {
        for url in [
            "https://t.bilibili.com/1245284537985925159",
            "t.bilibili.com/1245284537985925159",
            "https://t.bilibili.com/h5/dynamic/detail/1245284537985925159",
            "https://www.bilibili.com/opus/1245284537985925159",
            "https://m.bilibili.com/dynamic/1245284537985925159",
            "https://www.bilibili.com/opus/1245284537985925159?share_source=copy_web",
        ] {
            assert!(PATTERN.is_match(url), "{url}");
        }
    }

    #[test]
    fn pattern_rejects_other_bilibili_pages() {
        for url in [
            "https://www.bilibili.com/video/BV1JTtt6JEZu",
            "https://www.bilibili.com/",
            "https://space.bilibili.com/486906719/dynamic",
            "https://live.bilibili.com/22632424",
            "https://www.bilibili.com/read/cv123456",
            "https://t.bilibili.com/",
            "https://x.com/user/status/1234567890",
        ] {
            assert!(!PATTERN.is_match(url), "{url}");
        }
    }

    /// Short links stay unmatched on purpose (most point at videos) so the
    /// bot keeps ignoring them instead of answering with a failure.
    #[test]
    fn pattern_ignores_short_links() {
        assert!(!PATTERN.is_match("https://b23.tv/abc123"));
        assert_eq!(cache_key("https://b23.tv/abc123"), None);
    }

    #[test]
    fn cache_key_normalizes_direct_forms() {
        for url in [
            "https://t.bilibili.com/1245284537985925159",
            "https://t.bilibili.com/h5/dynamic/detail/1245284537985925159?utm_source=share",
            "https://www.bilibili.com/opus/1245284537985925159",
            "https://m.bilibili.com/dynamic/1245284537985925159",
        ] {
            assert_eq!(
                cache_key(url),
                Some("bilibili:1245284537985925159".to_string()),
                "{url}"
            );
        }
    }

    #[test]
    fn from_item_maps_draw_images_and_topic() {
        let fetched = parse(item_json(
            draw_item("http://i0.hdslb.com/bfs/new_dyn/a.jpg"),
            "新歌上线",
        ));

        assert_eq!(
            fetched.source_url,
            "https://www.bilibili.com/opus/1245284537985925159"
        );
        assert_eq!(fetched.site_id, "bilibili");
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "新歌上线");
        assert!(!fetched.sensitive);
        assert_eq!(fetched.media.len(), 1);
        match &fetched.media[0] {
            Media::Illustration {
                url,
                thumbnail_url,
                fallback_url,
                ..
            } => {
                assert_eq!(url, "https://i0.hdslb.com/bfs/new_dyn/a.jpg");
                assert_eq!(
                    thumbnail_url.as_deref(),
                    Some("https://i0.hdslb.com/bfs/new_dyn/a.jpg@518w.jpg")
                );
                assert_eq!(*fallback_url, None);
            }
            other => panic!("{other:?}"),
        }
        let caption = &fetched.caption;
        assert!(
            caption.starts_with("https://www.bilibili.com/opus/1245284537985925159\n"),
            "{caption}"
        );
        assert!(
            caption.contains("https://space.bilibili.com/486906719"),
            "{caption}"
        );
        assert!(caption.contains("索尼音乐中国"), "{caption}");
        assert!(caption.ends_with(": 新歌上线"), "{caption}");
        assert_eq!(
            fetched.render_fields(),
            Some((
                "索尼音乐中国",
                "https://space.bilibili.com/486906719",
                "",
                "新歌上线",
                "音乐"
            ))
        );
    }

    /// `.gif` sources are animations; they must not be sent as photos, and
    /// their thumbnail is left for Telegram to generate.
    #[test]
    fn from_item_maps_gif_as_animation() {
        let fetched = parse(item_json(
            draw_item("http://i0.hdslb.com/bfs/new_dyn/a.gif"),
            "",
        ));
        match &fetched.media[0] {
            Media::Animated {
                url, thumbnail_url, ..
            } => {
                assert_eq!(url, "https://i0.hdslb.com/bfs/new_dyn/a.gif");
                assert_eq!(thumbnail_url, "");
            }
            other => panic!("{other:?}"),
        }
        // No text: the caption is the link plus the author line only.
        assert_eq!(
            fetched.caption,
            "https://www.bilibili.com/opus/1245284537985925159\n<a href=\"https://space.bilibili.com/486906719\">索尼音乐中国</a>"
        );
    }

    /// Bilibili's media URLs arrive as `http://` or protocol-relative; both
    /// must become `https://` before they reach Telegram.
    #[test]
    fn media_urls_are_normalized_to_https() {
        let fetched = parse(item_json(draw_item("//i0.hdslb.com/bfs/new_dyn/p.jpg"), ""));
        assert_eq!(
            fetched.media[0].url(),
            "https://i0.hdslb.com/bfs/new_dyn/p.jpg"
        );

        let fetched = parse(item_json(draw_item("not-a-url"), ""));
        assert!(fetched.media.is_empty());
    }

    /// The serialization the web client asks for: an image/text post arrives
    /// as `major.opus` (pictures, body and optional headline) with **no**
    /// `desc` — the legacy shape would lose the text entirely.
    #[test]
    fn from_item_opus_shape_carries_text_and_pics() {
        let major = serde_json::json!({
            "type": "MAJOR_TYPE_OPUS",
            "opus": {
                "title": "每个人的青春里，都有一首 A-Lin",
                "summary": { "text": "那些曾经陪你失恋的歌\n\n【活动】详情见正文" },
                "pics": [
                    { "url": "http://i0.hdslb.com/bfs/new_dyn/a.jpg", "width": 2304, "height": 2880 },
                    { "url": "http://i0.hdslb.com/bfs/new_dyn/b.jpg", "width": 2304, "height": 2880 },
                ],
                "fold_action": ["展开", "收起"],
                "jump_url": "//www.bilibili.com/opus/1245284537985925159",
            },
        });
        let item = {
            let mut json = item_json(major, "");
            json["modules"]["module_dynamic"]["desc"] = serde_json::Value::Null;
            json
        };
        let fetched = parse(item);

        assert_eq!(fetched.title, "每个人的青春里，都有一首 A-Lin");
        assert_eq!(
            fetched.content,
            "那些曾经陪你失恋的歌\n\n【活动】详情见正文"
        );
        assert!(
            fetched.caption.contains("那些曾经陪你失恋的歌"),
            "{}",
            fetched.caption
        );
        assert_eq!(fetched.media.len(), 2);
        match &fetched.media[0] {
            Media::Illustration {
                url, thumbnail_url, ..
            } => {
                assert_eq!(url, "https://i0.hdslb.com/bfs/new_dyn/a.jpg");
                assert_eq!(
                    thumbnail_url.as_deref(),
                    Some("https://i0.hdslb.com/bfs/new_dyn/a.jpg@518w.jpg")
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fetched.media[1].url(),
            "https://i0.hdslb.com/bfs/new_dyn/b.jpg"
        );

        // A body without a headline, and a headline without a body, both
        // stand alone rather than rendering an empty line.
        for (title, body, expected) in [
            (None, "只有正文", ("", "只有正文")),
            (Some("只有标题"), "", ("只有标题", "")),
        ] {
            let major = serde_json::json!({
                "type": "MAJOR_TYPE_OPUS",
                "opus": { "title": title, "summary": { "text": body }, "pics": [] },
            });
            let mut json = item_json(major, "");
            json["modules"]["module_dynamic"]["desc"] = serde_json::Value::Null;
            let fetched = parse(json);
            assert_eq!((fetched.title.as_str(), fetched.content.as_str()), expected);
        }
    }

    /// The legacy shape stays supported: bilibili's `itemOpusStyle` flag is
    /// what moves the pictures to `major.opus.pics`, but `major.draw` items
    /// and a text-only `desc` must keep working if it is retired.
    #[test]
    fn from_item_legacy_draw_shape_still_parses() {
        let fetched = parse(item_json(
            draw_item("http://i0.hdslb.com/bfs/new_dyn/l.jpg"),
            "legacy 正文",
        ));
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "legacy 正文");
        assert_eq!(fetched.media.len(), 1);
        assert_eq!(
            fetched.media[0].url(),
            "https://i0.hdslb.com/bfs/new_dyn/l.jpg"
        );
    }

    /// The video stream is out of scope; an AV dynamic still yields its cover.
    #[test]
    fn from_item_maps_archive_cover() {
        let major = serde_json::json!({
            "type": "MAJOR_TYPE_ARCHIVE",
            "archive": {
                "aid": 117189055087158_i64,
                "bvid": "BV1JTtt6JEZu",
                "cover": "http://i0.hdslb.com/bfs/archive/c.jpg",
                "title": "Supersubmarina",
                "duration_text": "03:45",
            },
        });
        let fetched = parse(item_json(major, "投稿了视频"));
        assert_eq!(fetched.media.len(), 1);
        assert_eq!(
            fetched.media[0].url(),
            "https://i0.hdslb.com/bfs/archive/c.jpg"
        );
        assert!(matches!(fetched.media[0], Media::Illustration { .. }));
    }

    /// A 视频投稿动态 (`MAJOR_TYPE_ARCHIVE`) has **no body** — the API answers
    /// `desc: null` — so the archive card's title is the post's content and
    /// must fill `title` / `{title}` (regression: it used to stay empty).
    #[test]
    fn from_item_video_dynamic_uses_archive_title() {
        let major = serde_json::json!({
            "type": "MAJOR_TYPE_ARCHIVE",
            "archive": {
                "bvid": "BV1JTtt6JEZu",
                "cover": "http://i0.hdslb.com/bfs/archive/c.jpg",
                "title": "GTX760游戏性能测试，二手显卡尚能战否？",
                "desc": "入手一张2GB显存的七彩虹GTX760",
            },
        });
        let item = {
            let mut json = item_json(major, "");
            json["modules"]["module_dynamic"]["desc"] = serde_json::Value::Null;
            json
        };
        let fetched = parse(item.clone());

        assert_eq!(fetched.title, "GTX760游戏性能测试，二手显卡尚能战否？");
        assert!(
            fetched
                .caption
                .ends_with(": GTX760游戏性能测试，二手显卡尚能战否？"),
            "{}",
            fetched.caption
        );
        assert_eq!(
            fetched.render_fields().unwrap().2,
            "GTX760游戏性能测试，二手显卡尚能战否？"
        );
        assert_eq!(fetched.content, "");
        assert_eq!(fetched.media.len(), 1);

        // Forwarding a video dynamic: the quoted card title lands after the
        // `//@` marker, and the quoted cover becomes the media.
        let mut forward = item_json(serde_json::Value::Null, "");
        forward["modules"]["module_dynamic"]["desc"] = serde_json::Value::Null;
        forward["orig"] = item;
        let fetched = parse(forward);
        assert_eq!(fetched.title, "");
        assert_eq!(
            fetched.content,
            "//@索尼音乐中国:\nGTX760游戏性能测试，二手显卡尚能战否？"
        );
        assert_eq!(
            fetched.media[0].url(),
            "https://i0.hdslb.com/bfs/archive/c.jpg"
        );
    }

    /// A forward shell carries the quote's text and, when it has no media of
    /// its own, the quote's images.
    #[test]
    fn from_item_forward_uses_orig_media_and_text() {
        let mut json = item_json(serde_json::Value::Null, "转发理由");
        json["orig"] = serde_json::json!({
            "id_str": "1246767523595026450",
            "modules": {
                "module_author": { "name": "A-SOUL_Official", "mid": 703007996 },
                "module_dynamic": {
                    "desc": { "text": "原动态正文" },
                    "major": draw_item("http://i0.hdslb.com/bfs/new_dyn/o.jpg"),
                },
            },
        });
        let fetched = parse(json);

        assert_eq!(fetched.media.len(), 1);
        assert_eq!(
            fetched.media[0].url(),
            "https://i0.hdslb.com/bfs/new_dyn/o.jpg"
        );
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "转发理由\n//@A-SOUL_Official:\n原动态正文");
        // The forwarder stays the author; the quote appears in the text.
        assert!(
            fetched.caption.contains("索尼音乐中国"),
            "{}",
            fetched.caption
        );
        assert!(
            fetched.caption.contains("原动态正文"),
            "{}",
            fetched.caption
        );
    }

    /// A text-only dynamic has no media — the bot replies "No media found".
    #[test]
    fn from_item_without_major_has_no_media() {
        let fetched = parse(item_json(serde_json::Value::Null, "只有文字"));
        assert!(fetched.media.is_empty());
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "只有文字");
    }

    #[test]
    fn caption_escapes_site_text() {
        let fetched = parse(item_json(
            draw_item("http://i0.hdslb.com/bfs/new_dyn/a.jpg"),
            "<b>\"x\" & y</b>",
        ));
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "<b>\"x\" & y</b>");
        // `encode_text` escapes markup only; a bare quote is text, not an
        // attribute delimiter, and stays as-is.
        assert!(
            fetched.caption.contains("&lt;b&gt;\"x\" &amp; y&lt;/b&gt;"),
            "{}",
            fetched.caption
        );
        let (_, _, _, content, _) = fetched.render_fields().unwrap();
        assert_eq!(content, "&lt;b&gt;\"x\" &amp; y&lt;/b&gt;");
    }

    #[test]
    fn code_error_classifies_api_codes() {
        assert!(code_error(0, "0").is_none());
        // Risk control must be retryable so the queue backs off instead of
        // dropping the post.
        for code in [-352, -412] {
            let err = code_error(code, "-352").unwrap();
            assert!(is_retryable(&err), "{err}");
        }
        // A removed dynamic is permanent.
        assert!(matches!(code_error(500, ""), Some(FetchError::NotFound)));
        assert!(matches!(
            code_error(4101147, ""),
            Some(FetchError::NotFound)
        ));
        let err = code_error(-400, "param parsing failed").unwrap();
        assert!(!is_retryable(&err), "{err}");
        assert!(err.to_string().contains("-400"), "{err}");
    }

    #[test]
    fn buvid_cookie_needs_device_ids() {
        let fingerprint: model::Fingerprint = serde_json::from_value(serde_json::json!({
            "code": 0,
            "data": { "b_3": "ABCinfoc", "b_4": "DEF-Au1eCYnrGyhSrD" },
        }))
        .unwrap();
        assert_eq!(
            buvid_cookie(&fingerprint).as_deref(),
            Some("buvid3=ABCinfoc; buvid4=DEF-Au1eCYnrGyhSrD")
        );

        // A response without device ids must not produce a `Cookie` header
        // with empty values.
        for json in [
            serde_json::json!({ "code": 0, "data": { "b_3": "", "b_4": "" } }),
            serde_json::json!({ "code": -352 }),
        ] {
            let fingerprint: model::Fingerprint = serde_json::from_value(json).unwrap();
            assert_eq!(buvid_cookie(&fingerprint), None);
        }
    }

    /// The device-cookie half of the adapter: the fingerprint endpoint keeps
    /// answering even when the dynamic endpoint risk-controls this IP, so it
    /// stays a meaningful live check on its own.
    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to api.bilibili.com"]
    async fn live_fingerprint_yields_device_cookies() {
        let cookie = fetch_buvid().await.unwrap();
        let cookie = cookie.expect("fingerprint endpoint returned no device ids");
        assert!(cookie.contains("buvid3="), "{cookie}");
        assert!(cookie.contains("buvid4="), "{cookie}");
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to api.bilibili.com"]
    async fn live_fetch_draw_dynamic() {
        // 索尼音乐中国, a two-picture dynamic.
        let Some(fetched) = live_fetch("https://www.bilibili.com/opus/1245284537985925159").await
        else {
            return;
        };
        assert_eq!(fetched.site_id, "bilibili");
        assert_eq!(
            fetched.source_url,
            "https://www.bilibili.com/opus/1245284537985925159"
        );
        let urls: Vec<&str> = fetched.media.iter().map(|m| m.url()).collect();
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(
            urls.iter().all(|u| u.starts_with("https://i0.hdslb.com/")),
            "{urls:?}"
        );
        assert!(
            fetched.caption.contains("space.bilibili.com"),
            "{}",
            fetched.caption
        );
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to api.bilibili.com"]
    async fn live_fetch_text_only_dynamic() {
        // A text-only dynamic: no media, so the bot answers "No media found".
        let Some(fetched) = live_fetch("https://t.bilibili.com/1246767523595026450").await else {
            return;
        };
        assert!(fetched.media.is_empty());
        assert!(fetched.title.is_empty());
        assert!(!fetched.content.trim().is_empty());
    }

    /// Regression for the reported case: this opus post's legacy
    /// serialization has `desc: null`, so without the `itemOpusStyle`
    /// request it parsed with no text at all.
    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to api.bilibili.com"]
    async fn live_fetch_opus_dynamic_has_content() {
        let Some(fetched) = live_fetch("https://www.bilibili.com/opus/1248857553488576532").await
        else {
            return;
        };
        assert_eq!(fetched.title, "");
        assert_eq!(fetched.content, "[doge_金箍]黑白搭配");
        assert!(fetched.caption.ends_with("黑白搭配"), "{}", fetched.caption);
        let urls: Vec<&str> = fetched.media.iter().map(|m| m.url()).collect();
        assert_eq!(urls.len(), 1, "{urls:?}");
        assert!(urls[0].starts_with("https://i0.hdslb.com/"), "{urls:?}");
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to api.bilibili.com"]
    async fn live_fetch_video_dynamic_uses_archive_title() {
        // 索尼音乐中国's AV dynamic: no body, so the video title is the text.
        let Some(fetched) = live_fetch("https://t.bilibili.com/1248717597691609105").await else {
            return;
        };
        assert!(!fetched.title.trim().is_empty(), "{fetched:?}");
        assert!(fetched.content.is_empty(), "{fetched:?}");
        assert!(
            fetched.caption.contains(&fetched.title),
            "{}",
            fetched.caption
        );
        let urls: Vec<&str> = fetched.media.iter().map(|m| m.url()).collect();
        assert_eq!(urls.len(), 1, "{urls:?}");
        assert!(urls[0].contains("/bfs/archive/"), "{urls:?}");
    }

    /// Fetches a live dynamic, skipping the assertion when bilibili
    /// risk-controls this IP (the site blocks datacenter/over-used addresses
    /// with `-352` regardless of cookies — a real failure would surface as a
    /// parse error or a not-found instead). Mirrors the token-gated pixiv
    /// tests' "skipping: …" convention.
    async fn live_fetch(url: &str) -> Option<Fetched> {
        match fetch_from_url(url).await {
            Ok(fetched) => Some(fetched),
            Err(e) if e.to_string().contains("risk control") => {
                eprintln!("skipping: {e}");
                None
            }
            Err(e) => panic!("{e}"),
        }
    }
}
