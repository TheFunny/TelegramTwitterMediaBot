use super::model::{IllustrationModel, TypeModel};
use crate::media::Media;
use crate::site::{FetchError, Fetched};
use html_escape::encode_text;
use regex::Regex;
use std::sync::LazyLock;

pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?://)?(?:www\.)?pixiv\.net/(?:en/)?(?:(?:i|artworks)/|member_illust\.php\?(?:mode=[a-z_]*&)?illust_id=)(\d+)").unwrap()
});

pub fn enabled() -> bool {
    super::api::enabled()
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

#[derive(Debug)]
pub struct Illustration {
    id: String,
    title: String,
    author: String,
    author_id: String,
    tags: Vec<String>,
    pub(crate) media: Vec<Media>,
    nsfw: bool,
    /// Keeps a temp dir (ugoira MP4) alive until the send completes.
    pub(crate) _keep_alive: Option<tempfile::TempDir>,
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
            url = self.url(),
            title = encode_text(&self.title),
            author_url = self.author_url(),
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
        if matches!(&model.r#type, TypeModel::Ugoira) {
            // No static images for ugoira; the fetch path encodes an MP4 via
            // ffmpeg and appends it as a Video item (api.rs). This fallback
            // keeps media empty when encoding fails or ffmpeg is missing.
        } else if model.page_count > 1 {
            media.extend(model.meta_pages.iter().filter_map(|page| {
                page.image_urls
                    .original
                    .clone()
                    .map(|original| Media::Illustration {
                        title: None,
                        url: original,
                        thumbnail_url: Some(page.image_urls.medium.clone()),
                        fallback_url: Some(page.image_urls.large.clone()),
                    })
            }));
        } else if let Some(original) = model
            .meta_single_page
            .original_image_url
            .clone()
            .or(model.image_urls.original.clone())
        {
            media.push(Media::Illustration {
                title: None,
                url: original,
                thumbnail_url: Some(model.image_urls.medium.clone()),
                fallback_url: Some(model.image_urls.large.clone()),
            });
        }
        let nsfw = model.sanity_level > 5;
        Self {
            id,
            title,
            author,
            author_id,
            tags,
            media,
            nsfw,
            _keep_alive: None,
        }
    }
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
            tags: encode_text(&tags).into_owned(),
        });
        Fetched {
            source_url: url,
            caption: illustration.caption(),
            title: illustration.title.clone(),
            media: illustration.media,
            sensitive: illustration.nsfw,
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
    fn single_page_without_any_original_is_empty() {
        let v = illust_json("illust", 1, None, None, vec![], 0);
        let fetched: Fetched = parse(v).into();
        assert!(fetched.media.is_empty());
    }

    #[test]
    fn multi_page_skips_pages_without_original() {
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
        assert_eq!(fetched.media.len(), 1);
        match &fetched.media[0] {
            Media::Illustration {
                url,
                thumbnail_url,
                fallback_url,
                ..
            } => {
                assert_eq!(url, "https://i.pximg.net/p2.jpg");
                assert_eq!(thumbnail_url.as_deref(), Some("m2.jpg"));
                assert_eq!(fallback_url.as_deref(), Some("l2.jpg"));
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
        assert_eq!(fetched.site_name(), "pixiv");
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
