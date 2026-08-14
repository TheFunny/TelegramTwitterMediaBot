use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched, Site, SiteFuture};
use html_escape::{encode_double_quoted_attribute, encode_text};
use regex::Regex;
use std::sync::LazyLock;

/// Registry entry for the twitter adapter (see [`crate::site::Site`]).
pub struct TwitterSite;

impl Site for TwitterSite {
    fn id(&self) -> &'static str {
        "twitter"
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
    Regex::new(r"^(?:https?://)?(?:www\.|mobile\.)?(?:x|twitter|fixvx|vxtwitter|fixupx|fxtwitter)\.com/[^.]+/status/(\d+)").unwrap()
});

pub fn enabled() -> bool {
    true
}

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let id = PATTERN
        .captures(url)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .ok_or(FetchError::NotFound)?;
    match fetch(id).await {
        Ok(tweet) => Ok(tweet.into()),
        // Syndication withholds NSFW/age-restricted tweets (empty `{}`).
        // Retry as the logged-in user when TWITTER_AUTH_TOKEN is set;
        // otherwise degrade to an empty result (the bot replies
        // "No media found").
        Err(FetchError::Sensitive) => {
            if super::auth::enabled() {
                match super::auth::fetch(id).await {
                    Ok(tweet) => Ok(tweet.into()),
                    // The tweet is genuinely gone (deleted / suspended /
                    // tombstoned): report it instead of degrading to an
                    // empty result ("No media found"). Only unexpected
                    // fallback failures (network, parse) keep the NSFW
                    // placeholder.
                    Err(FetchError::NotFound) => Err(FetchError::NotFound),
                    Err(e) => {
                        log::warn!("twitter auth fallback failed for {id}: {e}");
                        Ok(empty_fetched(url))
                    }
                }
            } else {
                log::debug!("tweet {id} is sensitive; set TWITTER_AUTH_TOKEN to fetch NSFW media");
                Ok(empty_fetched(url))
            }
        }
        Err(e) => Err(e),
    }
}

/// Cache key for a twitter URL: `"twitter:<id>"`. The prefix is the site id
/// used for caption-format lookup and link-cache keys.
pub fn cache_key(url: &str) -> Option<String> {
    PATTERN
        .captures(url)
        .map(|caps| format!("twitter:{}", &caps[1]))
}

/// Twitter's fetch-retry policy: transient classes only. Not-found, blocked,
/// sensitive (NSFW withholding) and parse failures are permanent — retrying
/// them only wastes attempts against the syndication endpoint.
pub fn is_retryable(err: &FetchError) -> bool {
    matches!(err, FetchError::Http(_) | FetchError::Transient(_))
}

/// twimg URLs need no extra headers (no hotlink protection).
pub fn media_headers(_url: &str) -> Option<Vec<(&'static str, String)>> {
    None
}

/// A Fetched with no media for withheld tweets: the bot replies
/// "No media found" and moves on instead of erroring.
fn empty_fetched(url: &str) -> Fetched {
    Fetched {
        source_url: url.to_string(),
        // The raw user-supplied URL goes into an HTML caption; escape it so
        // crafted links cannot break the parse (Telegram 400).
        caption: encode_text(url).into_owned(),
        title: String::new(),
        media: vec![],
        sensitive: true,
        site_id: "twitter",
        render_data: None,
        _keep_alive: None,
    }
}

/// Fetches a tweet from the syndication endpoint. Deleted/blocked tweets
/// surface as `FetchError::NotFound`; withheld content (empty tombstone,
/// age-restricted) as `FetchError::Sensitive`.
pub async fn fetch(id: &str) -> Result<Tweet, FetchError> {
    let id_num = id.parse::<u64>().map_err(|_| FetchError::NotFound)?;
    let response = crate::site::CLIENT
        .get(format!(
            "https://cdn.syndication.twimg.com/tweet-result?id={id}&lang=en&token={}",
            syndication_token(id_num)
        ))
        .send()
        .await?;
    // 404/410 = gone (permanent); 429/5xx = transient and retried by fetch.
    let status = response.status();
    if !status.is_success() {
        return match status.as_u16() {
            404 | 410 => Err(FetchError::NotFound),
            _ => Err(FetchError::Transient(format!("twitter status {status}"))),
        };
    }
    let text = response.text().await?;
    // Classify before parsing the tweet (see [`parse_syndication_body`]).
    parse_syndication_body(&text)?;
    Tweet::from_syndication_json(&text).map_err(FetchError::Json)
}

/// Parses and classifies a syndication response body. `Ok` means the body is
/// a real tweet payload; `Err` carries the permanent error class:
/// - `NotFound`: an `errors` array (deleted/blocked) or a `TweetTombstone`
///   **with a reason** — "This Post was deleted by the Post author." /
///   "This Post is from a suspended account." (the tweet is gone).
/// - `Sensitive`: content withheld **without a deletion reason** — the empty
///   `{}` shape or an *empty* `TweetTombstone` (`{"__typename":
///   "TweetTombstone","tombstone":{}}`). Live tweets in restricted contexts
///   surface this way; treating them as deleted is a regression (a normal
///   tweet must not report "deleted"). Age-restricted tombstones route here
///   too so the logged-in GraphQL fallback can fetch the real tweet.
/// - `Json`: an unparseable body.
fn parse_syndication_body(text: &str) -> Result<serde_json::Value, FetchError> {
    let body: serde_json::Value = serde_json::from_str(text)?;
    if body.get("errors").is_some() {
        return Err(FetchError::NotFound);
    }
    if let Some(tombstone) = body.get("tombstone") {
        // Only a tombstone with an explicit reason means the tweet is gone;
        // a missing reason (empty `tombstone: {}`) or an age-restricted
        // reason means the tweet exists but is withheld.
        let reason = tombstone
            .get("text")
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if reason.is_empty() || reason.to_ascii_lowercase().contains("age-restricted") {
            return Err(FetchError::Sensitive);
        }
        return Err(FetchError::NotFound);
    }
    if body.get("id_str").is_none() {
        return Err(FetchError::Sensitive);
    }
    Ok(body)
}

/// The syndication token: JS `((id / 1e15) * PI).toString(36)` (the
/// `replace('0.','')` is a no-op for realistic tweet ids). The endpoint
/// currently serves public tweets regardless of the token; the formula is
/// kept for parity with the known-good client behavior.
fn syndication_token(id: u64) -> String {
    let value = (id as f64 / 1e15) * std::f64::consts::PI;
    let integer = value.trunc() as u64;
    let mut fraction = value.fract();
    let mut digits = String::new();
    if integer == 0 {
        digits.push('0');
    } else {
        let mut n = integer;
        let mut buf = Vec::new();
        while n > 0 {
            buf.push(char::from_digit((n % 36) as u32, 36).unwrap());
            n /= 36;
        }
        digits.extend(buf.into_iter().rev());
    }
    digits.push('.');
    for _ in 0..10 {
        fraction *= 36.0;
        let digit = fraction.trunc() as u32;
        digits.push(char::from_digit(digit.min(35), 36).unwrap());
        fraction -= digit as f64;
        if fraction == 0.0 {
            break;
        }
    }
    digits
}

#[derive(Debug)]
pub struct Tweet {
    id: String,
    text: String,
    author: String,
    author_id: String,
    media: Vec<Media>,
    sensitive: bool,
}

impl Tweet {
    fn url(&self) -> String {
        format!("{}/status/{}", self.author_url(), self.id)
    }

    fn author_url(&self) -> String {
        format!("https://x.com/{}", self.author_id)
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

    pub fn from_syndication_json(raw_json: &str) -> Result<Self, serde_json::Error> {
        let json: model::SyndicationTweet = serde_json::from_str(raw_json)?;
        let id = json.id_str;
        // Expand the user's t.co short links to their real destinations and
        // strip the appended media short link, mirroring FxEmbed's linkFixer
        // (no display_text_range arithmetic — see expand_links).
        let text = expand_links(&json.text, &json.entities.urls);
        // `name` is the display name, `screen_name` the handle (Python's
        // vxtwitter mapping: author = display name, author_id = handle).
        let author = json.user.name;
        let author_id = json.user.screen_name;
        let mut media = vec![];
        for item in json.media_details {
            match item.media_type.as_str() {
                "photo" => media.push(Media::Illustration {
                    title: None,
                    url: original_twimg_url(&item.media_url_https),
                    thumbnail_url: None,
                    // The param-less base URL is a reduced-size variant;
                    // used as the fallback when the original is too large.
                    fallback_url: Some(item.media_url_https.clone()),
                }),
                "video" => media.push(Media::Video {
                    title: None,
                    url: mp4_variant(&item),
                    thumbnail_url: item.media_url_https,
                }),
                "animated_gif" => media.push(Media::Animated {
                    title: None,
                    url: mp4_variant(&item),
                    thumbnail_url: item.media_url_https,
                }),
                _ => {}
            }
        }
        let sensitive = json.possibly_sensitive.unwrap_or(false);
        Ok(Self {
            id,
            text,
            author,
            author_id,
            media,
            sensitive,
        })
    }
}

/// Mirrors FxEmbed's `linkFixer` (link-fixer.ts): expand every t.co short
/// link that has an entity mapping to its real destination, drop internal
/// `x.com/i/web/status/…` plumbing links, then strip any remaining t.co
/// short link (the appended media link and other unmapped short links).
/// Pure content matching — no `display_text_range` arithmetic, so the
/// endpoint's inconsistent index units (UTF-16 vs code points, see the
/// deleted `strip_trailing_short_links`) never matter.
fn expand_links(text: &str, urls: &[model::SyndicationEntityUrl]) -> String {
    let mut out = text.to_string();
    for entity in urls {
        let Some(expanded) = &entity.expanded_url else {
            continue;
        };
        let replacement = if WEB_STATUS_URL.is_match(expanded) {
            ""
        } else {
            expanded
        };
        out = out.replace(&entity.url, replacement);
    }
    TCO_LINK.replace_all(&out, "").into_owned()
}

/// Internal x.com page links (reply / quote plumbing) expand to
/// `x.com/i/web/status/<id>`; FxEmbed drops them — the tweet's own content
/// already carries the information.
static WEB_STATUS_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://(?:x\.com|twitter\.com)/i/web/status/\w+").unwrap());

/// A t.co short link, optionally preceded by a space. Any leftover
/// occurrence (unmapped — e.g. the appended media link) is removed,
/// mirroring FxEmbed. Real short-link codes are 10 alphanumerics; the
/// length-agnostic class keeps fixtures and hypothetical odd lengths safe.
static TCO_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" ?https?://t\.co/[A-Za-z0-9]+").unwrap());

/// pbs.twimg.com serves a reduced default size without size params; `name=orig`
/// returns the original file (fxtwitter used to hand out the original
/// directly, the syndication API does not). Non-twimg URLs pass through
/// unchanged.
fn original_twimg_url(url: &str) -> String {
    if url.starts_with("https://pbs.twimg.com/") && (url.ends_with(".jpg") || url.ends_with(".png"))
    {
        format!("{url}?name=orig")
    } else {
        url.to_string()
    }
}

fn mp4_variant(item: &model::SyndicationMedia) -> String {
    item.video_info
        .as_ref()
        .and_then(|info| {
            info.variants
                .iter()
                .find(|variant| variant.content_type == "video/mp4")
        })
        .map(|variant| variant.url.clone())
        .unwrap_or_else(|| item.media_url_https.clone())
}

impl From<Tweet> for Fetched {
    fn from(tweet: Tweet) -> Self {
        let url = tweet.url();
        let author_url = tweet.author_url();
        let render_data = Some(crate::site::RenderData {
            url: url.clone(),
            author: encode_text(&tweet.author).into_owned(),
            author_url: author_url.clone(),
            title: encode_text(&tweet.text).into_owned(),
            tags: String::new(),
        });
        Fetched {
            source_url: url,
            caption: tweet.caption(),
            title: tweet.text.clone(),
            media: tweet.media,
            sensitive: tweet.sensitive,
            site_id: "twitter",
            render_data,
            _keep_alive: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(media_details: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "__typename": "Tweet",
            "id_str": "861627479294746624",
            "text": "a & b <c>",
            "user": { "name": "Display Name", "screen_name": "author_handle" },
            "possibly_sensitive": true,
            "mediaDetails": media_details
        })
    }

    #[test]
    fn pattern_matches_all_domains() {
        for url in [
            "https://x.com/user/status/1234567890",
            "https://twitter.com/user/status/1234567890",
            "https://mobile.twitter.com/user/status/1234567890",
            "https://www.x.com/user/status/1234567890",
            "https://fxtwitter.com/user/status/1234567890",
            "https://fixupx.com/user/status/1234567890",
            "https://fixvx.com/user/status/1234567890",
            "https://vxtwitter.com/user/status/1234567890",
        ] {
            let caps = PATTERN.captures(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(caps.get(1).unwrap().as_str(), "1234567890");
        }
    }

    #[test]
    fn pattern_rejects_non_tweet_urls() {
        for url in [
            "https://x.com/user",
            "https://x.com/user/status/abc",
            "https://bsky.app/profile/u/post/3xxxx",
            "https://pixiv.net/artworks/123",
            "https://example.com/x.com/user/status/123",
        ] {
            assert!(!PATTERN.is_match(url), "{url}");
        }
    }

    #[test]
    fn cache_key_prefixes_tweet_id() {
        assert_eq!(
            cache_key("https://x.com/user/status/1234567890"),
            Some("twitter:1234567890".into())
        );
        assert_eq!(cache_key("https://example.com/1"), None);
    }

    #[test]
    fn is_retryable_classifies_transient_and_permanent() {
        // Transient: network errors and explicit transient statuses (the
        // `Http` arm shares this match arm with `Transient`).
        assert!(is_retryable(&FetchError::Transient("429".into())));
        // Permanent: gone, blocked, withheld, oversized, unparseable.
        assert!(!is_retryable(&FetchError::NotFound));
        assert!(!is_retryable(&FetchError::Blocked));
        assert!(!is_retryable(&FetchError::Sensitive));
        assert!(!is_retryable(&FetchError::TooLarge));
        let json_err = serde_json::from_str::<serde_json::Value>("x").unwrap_err();
        assert!(!is_retryable(&FetchError::Json(json_err)));
    }

    #[test]
    fn syndication_json_converts_to_fetched() {
        let raw = fixture(serde_json::json!([
            { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/photo.jpg" },
            {
                "type": "video",
                "media_url_https": "https://pbs.twimg.com/thumb.jpg",
                "video_info": {
                    "variants": [
                        { "content_type": "application/x-mpegURL", "url": "https://x.com/pl.m3u8" },
                        { "content_type": "video/mp4", "url": "https://video.twimg.com/v.mp4" }
                    ]
                }
            }
        ]));
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        let fetched: Fetched = tweet.into();
        assert_eq!(
            fetched.source_url,
            "https://x.com/author_handle/status/861627479294746624"
        );
        assert_eq!(fetched.title, "a & b <c>");
        assert!(fetched.sensitive);
        assert_eq!(fetched.media.len(), 2);
        match &fetched.media[0] {
            Media::Illustration { url, .. } => {
                // Photo URL is rewritten to request the original file.
                assert_eq!(url, "https://pbs.twimg.com/media/photo.jpg?name=orig");
            }
            other => panic!("expected illustration, got {other:?}"),
        }
        match &fetched.media[1] {
            Media::Video {
                url, thumbnail_url, ..
            } => {
                assert_eq!(url, "https://video.twimg.com/v.mp4");
                assert_eq!(thumbnail_url, "https://pbs.twimg.com/thumb.jpg");
            }
            other => panic!("expected video, got {other:?}"),
        }
        assert!(
            fetched.caption.contains(
                "<a href=\"https://x.com/author_handle\">Display Name</a>: a &amp; b &lt;c&gt;"
            ),
            "caption: {}",
            fetched.caption
        );
    }

    #[test]
    fn syndication_text_only_has_no_media() {
        let raw = fixture(serde_json::json!([]));
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        let fetched: Fetched = tweet.into();
        assert!(fetched.media.is_empty());
    }

    #[test]
    fn syndication_gif_maps_to_animated() {
        let raw = fixture(serde_json::json!([
            {
                "type": "animated_gif",
                "media_url_https": "https://pbs.twimg.com/g.jpg",
                "video_info": {
                    "variants": [{ "content_type": "video/mp4", "url": "https://video.twimg.com/g.mp4" }]
                }
            }
        ]));
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert!(matches!(&tweet.media[0], Media::Animated { .. }));
    }

    #[test]
    fn syndication_text_strips_trailing_media_short_link() {
        // Real syndication shape: the appended media short link sits after the
        // visible text; the unmapped t.co link is stripped by content.
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": "hello world https://t.co/abc123",
            "user": { "name": "N", "screen_name": "h" },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, "hello world");
        assert!(!tweet.caption().contains("t.co"));
    }

    #[test]
    fn syndication_text_strips_trailing_link_regardless_of_index_units() {
        // Real tweet 2084567054481571919: the visible text is 30 code points
        // but 41 UTF-16 units, and the two endpoints historically reported
        // display_text_range in different units (UTF-16 on syndication, code
        // points on GraphQL). The FxEmbed-style content-based strip ignores
        // the range entirely, so the appended media link is removed for any
        // response shape.
        let text = "妄想𝑨𝒅𝒅𝒊𝒄𝒕𝒊𝒐𝒏…🩷💚❤️\n#ゼンゼロ　#zzzero https://t.co/XnIi83EkEB";
        let visible = "妄想𝑨𝒅𝒅𝒊𝒄𝒕𝒊𝒐𝒏…🩷💚❤️\n#ゼンゼロ　#zzzero";
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "2084567054481571919",
            "text": text,
            "user": { "name": "N", "screen_name": "h" },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, visible, "left a partial link");
        assert!(!tweet.caption().contains("t.co"));
    }

    #[test]
    fn syndication_text_strips_trailing_short_link_without_entities() {
        // No URL entities at all: the leftover t.co link is stripped by the
        // content regex.
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": "hello https://t.co/abc123",
            "user": { "name": "N", "screen_name": "h" },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, "hello");
    }

    #[test]
    fn syndication_text_expands_url_entities() {
        // Real FloodSocial shape: the user's own link is a t.co short link in
        // the text; the entity mapping expands it, the trailing media short
        // link is stripped.
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": "Test Tweet with @mentionThis $twtr https://t.co/RzmrQ6wAzD #hashtag https://t.co/9r69akA484",
            "user": { "name": "N", "screen_name": "h" },
            "entities": {
                "urls": [{
                    "url": "https://t.co/RzmrQ6wAzD",
                    "expanded_url": "http://bit.ly/2pUk4be",
                    "display_url": "bit.ly/2pUk4be"
                }]
            },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(
            tweet.text,
            "Test Tweet with @mentionThis $twtr http://bit.ly/2pUk4be #hashtag"
        );
        assert!(!tweet.caption().contains("t.co"));
    }

    #[test]
    fn syndication_text_strips_unmapped_short_links() {
        // FxEmbed parity: short links without an entity mapping (appended
        // media link, embedded unmapped links) are stripped, not kept.
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": "check https://t.co/abc123 #tag https://t.co/def456",
            "user": { "name": "N", "screen_name": "h" },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, "check #tag");
    }

    #[test]
    fn syndication_text_drops_internal_web_status_links() {
        // FxEmbed parity: a mapped link expanding to an internal
        // x.com/i/web/status/... page (reply/quote plumbing) is removed
        // instead of being shown.
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": "see https://t.co/xyz1234567 for context",
            "user": { "name": "N", "screen_name": "h" },
            "entities": {
                "urls": [{
                    "url": "https://t.co/xyz1234567",
                    "expanded_url": "https://x.com/i/web/status/9876543210",
                    "display_url": "x.com/i/web/status/9876543210"
                }]
            },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, "see  for context");
        assert!(!tweet.caption().contains("t.co"));
    }

    #[test]
    fn syndication_text_keeps_multibyte_text() {
        // Text-only tweet: no short links, the multibyte text is untouched.
        let text = "コミティア落ちたので、明日は行きません。🙏ごめんなさい";
        let units: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(units.len(), 28);
        let raw = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "1",
            "text": text,
            "user": { "name": "N", "screen_name": "h" },
            "mediaDetails": []
        });
        let tweet = Tweet::from_syndication_json(&raw.to_string()).unwrap();
        assert_eq!(tweet.text, text, "full text kept intact");
    }

    #[test]
    fn original_twimg_url_rewrites_photo_urls() {
        assert_eq!(
            original_twimg_url("https://pbs.twimg.com/media/C_UdnvPUwAE3Dnn.jpg"),
            "https://pbs.twimg.com/media/C_UdnvPUwAE3Dnn.jpg?name=orig"
        );
        assert_eq!(
            original_twimg_url("https://pbs.twimg.com/media/abc.png"),
            "https://pbs.twimg.com/media/abc.png?name=orig"
        );
        // Non-twimg URLs (videos, animated gifs) pass through unchanged.
        assert_eq!(
            original_twimg_url("https://video.twimg.com/v.mp4"),
            "https://video.twimg.com/v.mp4"
        );
        assert_eq!(
            original_twimg_url("https://pbs.twimg.com/media/abc.webp"),
            "https://pbs.twimg.com/media/abc.webp"
        );
    }

    #[test]
    fn syndication_token_matches_js_formula() {
        // JS: ((861627479294746624 / 1e15) * PI).toString(36) == "236.vrsocvda"
        let token = syndication_token(861627479294746624);
        assert!(token.starts_with("236.v"), "got {token}");
    }

    #[test]
    fn syndication_tombstone_maps_to_not_found() {
        // Deleted tweets answer HTTP 200 with a TweetTombstone carrying a
        // reason (no `errors`, no `id_str`); they must not fall through to
        // Sensitive, which would make the bot reply "No media found" for a
        // deleted tweet.
        let raw = serde_json::json!({
            "__typename": "TweetTombstone",
            "tombstone": {
                "text": { "rtl": false, "text": "This Post was deleted by the Post author. Learn more" }
            }
        });
        assert!(matches!(
            parse_syndication_body(&raw.to_string()),
            Err(FetchError::NotFound)
        ));
    }

    #[test]
    fn syndication_empty_tombstone_maps_to_sensitive() {
        // Regression: live tweets in restricted contexts answer with an
        // EMPTY tombstone (`{"__typename":"TweetTombstone","tombstone":{}}`)
        // — no deletion reason. They must not be reported as deleted.
        let raw = serde_json::json!({ "__typename": "TweetTombstone", "tombstone": {} });
        assert!(matches!(
            parse_syndication_body(&raw.to_string()),
            Err(FetchError::Sensitive)
        ));
    }

    #[test]
    fn syndication_age_restricted_tombstone_maps_to_sensitive() {
        // An age-restricted tombstone withholds a live tweet; route it to
        // the logged-in fallback instead of reporting it as gone.
        let raw = serde_json::json!({
            "__typename": "TweetTombstone",
            "tombstone": {
                "text": { "rtl": false, "text": "Age-restricted adult content" }
            }
        });
        assert!(matches!(
            parse_syndication_body(&raw.to_string()),
            Err(FetchError::Sensitive)
        ));
    }

    #[test]
    fn syndication_errors_maps_to_not_found() {
        // The classic gone shape: {"errors": [...]}.
        let raw = serde_json::json!({ "errors": [{ "message": "Couldn't find Tweet" }] });
        assert!(matches!(
            parse_syndication_body(&raw.to_string()),
            Err(FetchError::NotFound)
        ));
    }

    #[test]
    fn syndication_empty_object_maps_to_sensitive() {
        // NSFW / age-restricted withholding: an empty `{}`.
        assert!(matches!(
            parse_syndication_body("{}"),
            Err(FetchError::Sensitive)
        ));
    }

    #[test]
    fn syndication_tweet_body_passes() {
        let raw = fixture(serde_json::json!([]));
        assert!(parse_syndication_body(&raw.to_string()).is_ok());
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to cdn.syndication.twimg.com"]
    async fn live_fetch_with_photos() {
        let fetched = fetch("861627479294746624").await.unwrap();
        assert_eq!(fetched.media.len(), 4);
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to cdn.syndication.twimg.com"]
    async fn live_fetch_text_only() {
        let fetched = fetch("1992471125734142256").await.unwrap();
        assert!(fetched.media.is_empty());
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to cdn.syndication.twimg.com"]
    async fn live_fetch_deleted_tweet_is_not_found() {
        // Deleted tweet: the syndication endpoint answers with errors.
        let result = fetch("0").await;
        assert!(
            matches!(result, Err(FetchError::NotFound)),
            "got {result:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to cdn.syndication.twimg.com"]
    async fn live_fetch_tombstone_deleted_tweet_is_not_found() {
        // Regression: a real deleted tweet answering with a TweetTombstone
        // (HTTP 200, no errors/id_str) used to surface as Sensitive and
        // degrade to an empty result ("No media found").
        let result = fetch("2085948045967986859").await;
        assert!(
            matches!(result, Err(FetchError::NotFound)),
            "got {result:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to cdn.syndication.twimg.com"]
    async fn live_fetch_empty_tombstone_is_sensitive() {
        // Regression: a LIVE tweet (verified via a third-party API) answers
        // syndication with an empty TweetTombstone; it must surface as
        // Sensitive (withheld), never as NotFound (deleted).
        let result = fetch("2087851366253555752").await;
        assert!(
            matches!(result, Err(FetchError::Sensitive)),
            "got {result:?}"
        );
    }
}
