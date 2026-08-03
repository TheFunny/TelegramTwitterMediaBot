use super::model;
use crate::media::Media;
use crate::site::{FetchError, Fetched};
use html_escape::encode_text;
use regex::Regex;
use std::sync::LazyLock;

pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"bsky\.app/profile/([\w.\-:]+)/post/([\w.\-~]+)").unwrap()
});

pub fn enabled() -> bool {
    true
}

pub async fn fetch_from_url(url: &str) -> Result<Fetched, FetchError> {
    let caps = PATTERN.captures(url).ok_or(FetchError::NotFound)?;
    let handle = caps.get(1).map(|m| m.as_str()).ok_or(FetchError::NotFound)?;
    let rkey = caps.get(2).map(|m| m.as_str()).ok_or(FetchError::NotFound)?;
    Ok(fetch(handle, rkey).await?.into())
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
    let text = response.text().await?;
    Ok(Post::from_json(&text, rkey.to_string())?)
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
            url = self.url(),
            author_url = self.author_url(),
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
                                title: None,
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
                                title: None,
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
            title: encode_text(&post.text).into_owned(),
            tags: String::new(),
        });
        Fetched {
            source_url: url,
            caption: post.caption(),
            title: post.text.clone(),
            media: post.media,
            sensitive: post.sensitive,
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
        assert_eq!(fetched.source_url, "https://bsky.app/profile/user.bsky.social/post/3xxxx");
        assert_eq!(fetched.title, "hello <world>");
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

    #[tokio::test]
    async fn live_fetch_with_photos() {
        let fetched = fetch_from_url(
            "https://bsky.app/profile/asagi0398.bsky.social/post/3mqkhrq5w6k2m",
        )
        .await
        .unwrap();
        assert_eq!(
            fetched.source_url,
            "https://bsky.app/profile/asagi0398.bsky.social/post/3mqkhrq5w6k2m"
        );
        assert!(!fetched.caption.is_empty());
    }

    #[tokio::test]
    async fn live_fetch_smoke() {
        let fetched = fetch_from_url("https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224")
            .await
            .unwrap();
        assert_eq!(
            fetched.source_url,
            "https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224"
        );
        assert!(!fetched.caption.is_empty());
    }
}
