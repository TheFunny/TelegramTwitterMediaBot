impl Media {
    pub fn url(&self) -> &str {
        match self {
            Media::Illustration { url, .. } => url,
            Media::Video { url, .. } => url,
            Media::Animated { url, .. } => url,
        }
    }

    pub fn thumbnail_url(&self) -> Option<&str> {
        match self {
            Media::Illustration { thumbnail_url, .. } => thumbnail_url.as_deref(),
            Media::Video { thumbnail_url, .. } => Some(thumbnail_url),
            Media::Animated { thumbnail_url, .. } => Some(thumbnail_url),
        }
    }

    /// A smaller variant of this media's file (used as the fallback when the
    /// primary URL or upload exceeds Telegram's size limits). None when no
    /// smaller variant exists (videos, animated gifs).
    pub fn smaller_url(&self) -> Option<&str> {
        match self {
            Media::Illustration {
                url,
                fallback_url,
                thumbnail_url,
                ..
            } => fallback_url
                .as_deref()
                .or(thumbnail_url.as_deref())
                .filter(|smaller| *smaller != url),
            Media::Video { .. } | Media::Animated { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum Media {
    Illustration {
        url: String,
        thumbnail_url: Option<String>,
        fallback_url: Option<String>,
    },
    Video {
        url: String,
        thumbnail_url: String,
    },
    Animated {
        url: String,
        thumbnail_url: String,
    },
}
