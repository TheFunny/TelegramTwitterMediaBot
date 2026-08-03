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
}

#[derive(Debug)]
pub enum Media {
    Illustration {
        title: Option<String>,
        url: String,
        thumbnail_url: Option<String>,
        fallback_url: Option<String>,
    },
    Video {
        title: Option<String>,
        url: String,
        thumbnail_url: String,
    },
    Animated {
        title: Option<String>,
        url: String,
        thumbnail_url: String,
    },
}
