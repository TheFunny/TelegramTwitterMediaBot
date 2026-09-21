use super::model::{IllustrationModel, ImageUrlsModel};
use crate::media::Media;
use crate::site::{FetchError, Fetched, PixivError, Site, SiteFuture};
use html_escape::{encode_double_quoted_attribute, encode_text};
use regex::Regex;
use std::sync::LazyLock;

pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?://)?(?:www\.)?pixiv\.net/(?:en/)?(?:(?:i|artworks)/|member_illust\.php\?(?:mode=[a-z_]*&)?illust_id=)(\d+)").unwrap()
});

pub fn enabled() -> bool {
    super::api::enabled()
}

/// Registry entry for the pixiv adapter (see [`crate::site::Site`]).
pub struct PixivSite;

impl Site for PixivSite {
    fn id(&self) -> &'static str {
        "pixiv"
    }

    fn pattern(&self) -> &'static Regex {
        &PATTERN
    }

    fn enabled(&self) -> bool {
        enabled()
    }

    fn cache_key(&self, url: &str) -> Option<String> {
        cache_key(url)
    }

    fn fetch_from_url<'a>(&'a self, url: &'a str) -> SiteFuture<'a, Fetched> {
        Box::pin(async move { fetch_from_url(url).await })
    }

    fn is_retryable(&self, err: &FetchError) -> bool {
        is_retryable(err)
    }

    fn media_headers(&self, url: &str) -> Option<Vec<(&'static str, String)>> {
        media_headers(url)
    }

    fn validate(&self) -> SiteFuture<'static, (), String> {
        Box::pin(async { startup_validation(super::api::validate().await) })
    }
}

/// Turns the startup token exchange's outcome into what the bot reports, and
/// disables pixiv only for a rejected credential. A bad *moment* — a 5xx or a
/// network error while the container comes up — must not disable it: disabling
/// on any error turned every later pixiv link into "support is disabled".
/// Separate from the network call so the decision is testable.
fn startup_validation(result: Result<(), PixivError>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if pixiv_error_is_retryable(&e) => {
            Err(format!("{e} (transient — pixiv stays enabled)"))
        }
        Err(e) => {
            super::api::disable();
            Err(format!("{e}"))
        }
    }
}

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let id = PATTERN
        .captures(url)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .ok_or(FetchError::NotFound)?;
    let id = id.parse::<u64>().map_err(|_| FetchError::NotFound)?;
    Ok(super::api::fetch(id).await?.into())
}

/// Cache key for a pixiv URL: `"pixiv:<id>"`. The prefix is the site id used
/// for caption-format lookup and link-cache keys.
pub fn cache_key(url: &str) -> Option<String> {
    PATTERN
        .captures(url)
        .map(|caps| format!("pixiv:{}", &caps[1]))
}

/// Pixiv's fetch-retry policy: transient classes only — network errors and
/// HTTP 429/5xx. Permanent 4xx (bad/expired token, forbidden, not found),
/// API/auth errors, unparseable bodies and missing auth are not retried.
pub fn is_retryable(err: &FetchError) -> bool {
    match err {
        FetchError::Http(_) | FetchError::Transient(_) => true,
        FetchError::Pixiv(e) => pixiv_error_is_retryable(e),
        _ => false,
    }
}

/// The pixiv-specific half of the retry policy, shared with startup
/// validation: a bad moment (429/5xx, a network error) is retryable, a
/// rejected credential is not.
fn pixiv_error_is_retryable(err: &PixivError) -> bool {
    match err {
        PixivError::Http(_) => true,
        PixivError::Status(code) if *code == 429 || *code >= 500 => true,
        PixivError::Status(_) | PixivError::Api(_) | PixivError::Json(_) | PixivError::NoAuth => {
            false
        }
    }
}

/// pximg.net is hotlink-protected: downloads must carry the pixiv Referer.
/// The match is on the media host, not the site PATTERN — pixiv's PATTERN
/// only matches `pixiv.net/artworks/...`, never `i.pximg.net`.
pub fn media_headers(url: &str) -> Option<Vec<(&'static str, String)>> {
    if url.to_ascii_lowercase().contains("pximg.net") {
        Some(vec![("Referer", "https://www.pixiv.net/".to_string())])
    } else {
        None
    }
}

/// Flattens the app API's HTML description into plain text: `<br>` (and `<p>`)
/// become line breaks, other tags are dropped, entities decoded, the ends
/// trimmed. A caption shows text, not markup, so the author's `<a href>` links
/// contribute their link text only.
fn flatten_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        // Only `<` followed by `/` or a letter opens a tag — a bare `<` in
        // prose ("2 < 3") is text.
        let opens_tag = c == '<'
            && chars
                .peek()
                .is_some_and(|next| *next == '/' || next.is_ascii_alphabetic());
        if !opens_tag {
            out.push(c);
            continue;
        }
        let mut tag = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '>' {
                closed = true;
                break;
            }
            tag.push(c);
        }
        if !closed {
            // Unclosed `<…`: keep it as text rather than dropping the tail.
            out.push('<');
            out.push_str(&tag);
            break;
        }
        // `<br>`, `<br/>`, `<br />` with or without attributes, and both
        // halves of a paragraph break the line; everything else is dropped.
        let tag = tag
            .trim()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .trim()
            .to_ascii_lowercase();
        if tag == "p" || tag.starts_with("br") {
            out.push('\n');
        }
    }
    html_escape::decode_html_entities(&out).trim().to_string()
}

#[derive(Debug)]
pub struct Illustration {
    id: String,
    title: String,
    /// The artwork's description, HTML flattened to plain text.
    content: String,
    author: String,
    author_id: String,
    tags: Vec<String>,
    pub(crate) media: Vec<Media>,
    nsfw: bool,
    /// Keeps a temp dir (ugoira MP4) alive until the send completes.
    pub(crate) _keep_alive: Option<std::sync::Arc<tempfile::TempDir>>,
}

impl Illustration {
    fn url(&self) -> String {
        format!("https://www.pixiv.net/artworks/{}", self.id)
    }

    fn author_url(&self) -> String {
        format!("https://www.pixiv.net/users/{}", self.author_id)
    }

    pub fn caption(&self) -> String {
        format!(
            "<a href=\"{url}\">{title}</a> / <a href=\"{author_url}\">{author}</a>\n{tags}",
            url = encode_double_quoted_attribute(&self.url()),
            title = encode_text(&self.title),
            author_url = encode_double_quoted_attribute(&self.author_url()),
            author = encode_text(&self.author),
            tags = encode_text(
                &self
                    .tags
                    .iter()
                    .map(|tag| format!("#{tag}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )
    }

    pub fn from_model(model: &IllustrationModel) -> Self {
        let id = model.id.to_string();
        let title = model.title.clone();
        let content = flatten_html(&model.caption);
        let author = model.user.name.clone();
        let author_id = model.user.id.to_string();
        let mut tags: Vec<String> = model.tags.iter().map(|tag| tag.name.clone()).collect();
        // illust_ai_type: 0 = undefined, 1 = not AI, 2 = AI-generated.
        // Mark AI works with a leading #AI tag (rendered via the `#{tag}`
        // caption format).
        if model.illust_ai_type == 2 {
            tags.insert(0, "AI".to_string());
        }
        let mut media = vec![];
        if model.r#type == "ugoira" {
            // No static images for ugoira; the fetch path encodes an MP4 via
            // ffmpeg and appends it as a Video item (api.rs). This fallback
            // keeps media empty when encoding fails or ffmpeg is missing.
        } else if model.page_count > 1 {
            // Every page is kept: `original` is the only URL the API may leave
            // out (typically the restricted ones), and a page without it used
            // to be dropped whole — losing a page of the work while `large`
            // sat right there.
            media.extend(model.meta_pages.iter().filter_map(|page| {
                page_illustration(page.image_urls.original.clone(), &page.image_urls)
            }));
        } else {
            // The single page names its original in one of two places, and
            // `large` is the last resort.
            let urls = &model.image_urls;
            let original = model
                .meta_single_page
                .original_image_url
                .clone()
                .or_else(|| urls.original.clone());
            media.extend(page_illustration(original, urls));
        }
        let nsfw = model.sanity_level > 5;
        Self {
            id,
            title,
            content,
            author,
            author_id,
            tags,
            media,
            nsfw,
            _keep_alive: None,
        }
    }
}

/// One artwork page as a media item: `original` when the API sent one, else the
/// `large` variant (the same picture at a lower resolution), with `medium` as
/// the thumbnail. `None` when the API gave no usable URL at all.
fn page_illustration(original: Option<String>, urls: &ImageUrlsModel) -> Option<Media> {
    let url = original.unwrap_or_else(|| urls.large.clone());
    if url.is_empty() {
        return None;
    }
    Some(Media::Illustration {
        url,
        thumbnail_url: Some(urls.medium.clone()),
        fallback_url: Some(urls.large.clone()),
    })
}

impl From<Illustration> for Fetched {
    fn from(illustration: Illustration) -> Self {
        let url = illustration.url();
        let author_url = illustration.author_url();
        let tags = illustration
            .tags
            .iter()
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
            .join(" ");
        let render_data = Some(crate::site::RenderData {
            url: url.clone(),
            author: encode_text(&illustration.author).into_owned(),
            author_url: author_url.clone(),
            title: encode_text(&illustration.title).into_owned(),
            content: encode_text(&illustration.content).into_owned(),
            tags: encode_text(&tags).into_owned(),
        });
        Fetched {
            source_url: url,
            caption: illustration.caption(),
            title: illustration.title.clone(),
            content: illustration.content.clone(),
            media: illustration.media,
            sensitive: illustration.nsfw,
            site_id: "pixiv",
            render_data,
            _keep_alive: illustration._keep_alive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::IllustrationModel;
    use super::*;

    fn illust_json(
        type_: &str,
        page_count: u8,
        single_original: Option<&str>,
        image_urls_original: Option<&str>,
        pages: Vec<(Option<&str>, &str, &str)>,
        ai_type: i32,
    ) -> serde_json::Value {
        let meta_pages: Vec<serde_json::Value> = pages
            .into_iter()
            .map(|(original, medium, large)| {
                serde_json::json!({
                    "image_urls": {
                        "medium": medium,
                        "large": large,
                        "original": original
                    }
                })
            })
            .collect();
        serde_json::json!({
            "illust": {
                "id": 123,
                "title": "Art <title>",
                "caption": "一行说明<br />二行 <a href=\"https://x.example/\">链接</a> &amp; 结尾",
                "type": type_,
                "image_urls": {
                    "medium": "medium.jpg",
                    "large": "large.jpg",
                    "original": image_urls_original
                },
                "user": { "id": 456, "name": "Artist" },
                "tags": [{ "name": "tag1" }, { "name": "tag2" }],
                "page_count": page_count,
                "sanity_level": 6,
                "illust_ai_type": ai_type,
                "meta_single_page": { "original_image_url": single_original },
                "meta_pages": meta_pages
            }
        })
    }

    fn parse(v: serde_json::Value) -> Illustration {
        let model: IllustrationModel = serde_json::from_value(v["illust"].clone()).unwrap();
        Illustration::from_model(&model)
    }

    /// The description arrives as HTML and becomes plain-text content: breaks
    /// kept, tags dropped (links keep their text), entities decoded.
    #[test]
    fn from_json_maps_description_to_content() {
        let v = illust_json("illust", 1, None, Some("o.jpg"), vec![], 0);
        let illustration = parse(v);
        assert_eq!(illustration.content, "一行说明\n二行 链接 & 结尾");

        let fetched: Fetched = illustration.into();
        assert_eq!(fetched.title, "Art <title>");
        assert_eq!(fetched.content, "一行说明\n二行 链接 & 结尾");
        // The built-in caption keeps its layout: the description stays out of
        // it and is available through `{content}`.
        assert!(!fetched.caption.contains("一行说明"), "{}", fetched.caption);
        assert_eq!(
            fetched.render_fields().unwrap().3,
            "一行说明\n二行 链接 &amp; 结尾"
        );
        assert!(
            fetched
                .caption_with("{title}: {content}")
                .ends_with("一行说明\n二行 链接 &amp; 结尾")
        );
    }

    #[test]
    fn flatten_html_handles_common_markup() {
        assert_eq!(flatten_html(""), "");
        assert_eq!(flatten_html("plain"), "plain");
        assert_eq!(flatten_html("a<br />b<br/>c<br>d"), "a\nb\nc\nd");
        // A paragraph break is a blank line, exactly like `<br /><br />` —
        // writing it as one newline would flatten the author's paragraphs.
        assert_eq!(flatten_html("<p>one</p><p>two</p>"), "one\n\ntwo");
        assert_eq!(flatten_html("a &amp; b &lt;c&gt;"), "a & b <c>");
        // Nothing to strip: angle brackets that are not a tag survive.
        assert_eq!(flatten_html("2 < 3"), "2 < 3");
    }

    #[test]
    fn pattern_matches_all_forms() {
        let cases = [
            ("https://www.pixiv.net/artworks/123456", "123456"),
            ("https://pixiv.net/artworks/123456", "123456"),
            ("https://www.pixiv.net/en/artworks/123456", "123456"),
            ("https://www.pixiv.net/i/123456", "123456"),
            (
                "https://www.pixiv.net/member_illust.php?mode=medium&illust_id=123456",
                "123456",
            ),
            (
                "https://www.pixiv.net/en/member_illust.php?illust_id=123456",
                "123456",
            ),
        ];
        for (url, id) in cases {
            let caps = PATTERN.captures(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(caps.get(1).unwrap().as_str(), id);
        }
    }

    #[test]
    fn pattern_rejects_non_artwork_urls() {
        for url in [
            "https://www.pixiv.net/users/123",
            "https://x.com/user/status/123",
            "https://bsky.app/profile/u/post/3xxxx",
        ] {
            assert!(!PATTERN.is_match(url), "{url}");
        }
    }

    #[test]
    fn startup_validation_keeps_the_site_enabled_on_a_bad_moment() {
        use super::super::api;

        // The startup decision, not the retry policy: a 5xx/429 while the
        // container comes up must leave pixiv enabled and say so in the message
        // the admin gets. The rejected-credential half is not exercised here —
        // it calls `disable()`, a process-wide flag with no reset, so a test
        // touching it would order-couple every other pixiv test (the predicate
        // it keys on is covered by the table below).
        for err in [PixivError::Status(429), PixivError::Status(503)] {
            let enabled_before = api::enabled();
            let message = startup_validation(Err(err)).unwrap_err();
            assert!(message.contains("stays enabled"), "{message}");
            assert_eq!(
                api::enabled(),
                enabled_before,
                "a bad moment must not disable the site"
            );
        }
        assert!(startup_validation(Ok(())).is_ok());
    }

    #[test]
    fn is_retryable_classifies_transient_and_permanent() {
        // Transient: network errors, explicit transient, pixiv 429/5xx.
        assert!(is_retryable(&FetchError::Transient("429".into())));
        assert!(is_retryable(&FetchError::Pixiv(PixivError::Status(429))));
        assert!(is_retryable(&FetchError::Pixiv(PixivError::Status(500))));
        assert!(is_retryable(&FetchError::Pixiv(PixivError::Status(503))));
        // Permanent: pixiv 4xx (bad/expired token, forbidden, not found),
        // api/auth errors, unparseable bodies, not-found/blocked/sensitive.
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Status(400))));
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Status(401))));
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Status(403))));
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Status(404))));
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Api(
            "invalid_grant".into()
        ))));
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::NoAuth)));
        let json_err = serde_json::from_str::<serde_json::Value>("x").unwrap_err();
        assert!(!is_retryable(&FetchError::Pixiv(PixivError::Json(
            json_err
        ))));
        assert!(!is_retryable(&FetchError::NotFound));
        assert!(!is_retryable(&FetchError::Blocked));
        assert!(!is_retryable(&FetchError::Sensitive));
        assert!(!is_retryable(&FetchError::TooLarge));
    }

    #[test]
    fn media_headers_adds_referer_only_for_pximg() {
        assert_eq!(
            media_headers("https://i.pximg.net/img-original/img/1.png"),
            Some(vec![("Referer", "https://www.pixiv.net/".to_string())])
        );
        assert_eq!(media_headers("https://www.pixiv.net/artworks/1"), None);
        assert_eq!(media_headers("https://x.com/u/status/1"), None);
    }

    #[test]
    fn ugoira_yields_empty_media() {
        let v = illust_json(
            "ugoira",
            1,
            Some("https://i.pximg.net/orig.jpg"),
            None,
            vec![],
            0,
        );
        let illustration = parse(v);
        let fetched: Fetched = illustration.into();
        assert!(fetched.media.is_empty());
        assert!(fetched.sensitive, "sanity_level 6 > 5");
        assert_eq!(fetched.title, "Art <title>");
    }

    #[test]
    fn single_page_with_single_original() {
        let v = illust_json(
            "illust",
            1,
            Some("https://i.pximg.net/single.jpg"),
            None,
            vec![],
            0,
        );
        let fetched: Fetched = parse(v).into();
        assert_eq!(fetched.media.len(), 1);
        match &fetched.media[0] {
            Media::Illustration { url, .. } => {
                assert_eq!(url, "https://i.pximg.net/single.jpg")
            }
            other => panic!("expected Illustration, got {other:?}"),
        }
    }

    #[test]
    fn single_page_falls_back_to_image_urls_original() {
        let v = illust_json(
            "illust",
            1,
            None,
            Some("https://i.pximg.net/fallback.jpg"),
            vec![],
            0,
        );
        let fetched: Fetched = parse(v).into();
        assert_eq!(fetched.media.len(), 1);
        match &fetched.media[0] {
            Media::Illustration { url, .. } => {
                assert_eq!(url, "https://i.pximg.net/fallback.jpg")
            }
            other => panic!("expected Illustration, got {other:?}"),
        }
    }

    #[test]
    fn single_page_without_any_original_falls_back_to_large() {
        let v = illust_json("illust", 1, None, None, vec![], 0);
        let fetched: Fetched = parse(v).into();
        // Neither `meta_single_page.original_image_url` nor `image_urls.
        // original` is set: the work is still deliverable as `large`.
        match fetched.media.as_slice() {
            [Media::Illustration { url, .. }] => assert_eq!(url, "large.jpg"),
            other => panic!("expected the large variant, got {other:?}"),
        }
    }

    #[test]
    fn multi_page_keeps_pages_without_original() {
        let v = illust_json(
            "illust",
            2,
            None,
            None,
            vec![
                (None, "m1.jpg", "l1.jpg"),
                (Some("https://i.pximg.net/p2.jpg"), "m2.jpg", "l2.jpg"),
            ],
            0,
        );
        let fetched: Fetched = parse(v).into();
        // Both pages arrive: the restricted one (no `original`) sends its
        // `large` instead of vanishing — a dropped page is a missing picture.
        let urls: Vec<&str> = fetched
            .media
            .iter()
            .map(|media| match media {
                Media::Illustration { url, .. } => url.as_str(),
                other => panic!("expected Illustration, got {other:?}"),
            })
            .collect();
        assert_eq!(urls, vec!["l1.jpg", "https://i.pximg.net/p2.jpg"]);
        match &fetched.media[0] {
            Media::Illustration {
                thumbnail_url,
                fallback_url,
                ..
            } => {
                assert_eq!(thumbnail_url.as_deref(), Some("m1.jpg"));
                // `large` is the item itself here, so it is not also a
                // smaller variant of itself.
                assert_eq!(fallback_url.as_deref(), Some("l1.jpg"));
            }
            other => panic!("expected Illustration, got {other:?}"),
        }
    }

    #[test]
    fn caption_with_escapes_format_and_substitutes() {
        let v = illust_json(
            "illust",
            1,
            Some("https://i.pximg.net/o.jpg"),
            None,
            vec![],
            0,
        );
        let fetched: Fetched = parse(v).into();
        // Format string is escaped in full, then placeholders substituted.
        let out = fetched.caption_with("{title} by {author} <script> {tags}");
        assert!(
            out.contains("Art &lt;title&gt; by Artist &lt;script&gt; #tag1 #tag2"),
            "got: {out}"
        );
        assert!(!out.contains("<script>"), "no raw HTML injection: {out}");
        // {url} and {author_url} carry the site's own URLs.
        let out = fetched.caption_with("{url} {author_url}");
        assert_eq!(
            out,
            "https://www.pixiv.net/artworks/123 https://www.pixiv.net/users/456"
        );
        // Empty format falls back to the built-in caption.
        assert_eq!(fetched.caption_with(""), fetched.caption);
        assert_eq!(fetched.site_id, "pixiv");
    }

    #[test]
    fn ai_work_gets_leading_ai_tag() {
        // illust_ai_type == 2 is the only AI marker.
        let v = illust_json(
            "illust",
            1,
            Some("https://i.pximg.net/o.jpg"),
            None,
            vec![],
            2,
        );
        let fetched: Fetched = parse(v).into();
        assert!(
            fetched.caption.contains("#AI #tag1 #tag2"),
            "caption: {}",
            fetched.caption
        );
        // The {tags} placeholder reflects the tag array too.
        assert!(
            fetched.caption_with("{tags}").starts_with("#AI "),
            "got: {}",
            fetched.caption_with("{tags}")
        );
    }

    #[test]
    fn non_ai_work_has_no_ai_tag() {
        // 1 = explicitly not AI, 0 = undefined: neither gets the #AI tag.
        for ai_type in [0, 1] {
            let v = illust_json(
                "illust",
                1,
                Some("https://i.pximg.net/o.jpg"),
                None,
                vec![],
                ai_type,
            );
            let fetched: Fetched = parse(v).into();
            assert!(
                !fetched.caption.contains("#AI"),
                "ai_type={ai_type} got: {}",
                fetched.caption
            );
        }
    }

    #[test]
    fn caption_escapes_and_links() {
        let v = illust_json(
            "illust",
            1,
            Some("https://i.pximg.net/o.jpg"),
            None,
            vec![],
            0,
        );
        let fetched: Fetched = parse(v).into();
        assert!(
            fetched
                .caption
                .contains("<a href=\"https://www.pixiv.net/artworks/123\">Art &lt;title&gt;</a>"),
            "caption: {}",
            fetched.caption
        );
        assert!(fetched.caption.contains("#tag1 #tag2"));
        assert_eq!(fetched.source_url, "https://www.pixiv.net/artworks/123");
    }
}
