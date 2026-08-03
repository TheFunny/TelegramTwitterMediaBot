use serde::Deserialize;

/// Response shape of the syndication endpoint
/// (`cdn.syndication.twimg.com/tweet-result`).
#[derive(Deserialize, Debug)]
pub struct SyndicationTweet {
    pub id_str: String,
    pub text: String,
    pub user: SyndicationUser,
    #[serde(default)]
    pub possibly_sensitive: Option<bool>,
    /// Visible-text span; the raw `text` field has the appended media short
    /// link after it. Indices are UTF-16 code units.
    #[serde(default, rename = "display_text_range")]
    pub display_text_range: Option<[usize; 2]>,
    #[serde(default)]
    pub entities: SyndicationEntities,
    #[serde(default, rename = "mediaDetails")]
    pub media_details: Vec<SyndicationMedia>,
}

#[derive(Deserialize, Debug, Default)]
pub struct SyndicationEntities {
    #[serde(default)]
    pub urls: Vec<SyndicationEntityUrl>,
}

/// A URL entity: `url` is the t.co short link as it appears in the text,
/// `expanded_url` the real destination.
#[derive(Deserialize, Debug)]
pub struct SyndicationEntityUrl {
    pub url: String,
    #[serde(default)]
    pub expanded_url: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct SyndicationUser {
    pub name: String,
    pub screen_name: String,
}

#[derive(Deserialize, Debug)]
pub struct SyndicationMedia {
    #[serde(rename = "type")]
    pub media_type: String,
    pub media_url_https: String,
    #[serde(default)]
    pub video_info: Option<SyndicationVideoInfo>,
}

#[derive(Deserialize, Debug)]
pub struct SyndicationVideoInfo {
    #[serde(default)]
    pub variants: Vec<SyndicationVariant>,
}

#[derive(Deserialize, Debug)]
pub struct SyndicationVariant {
    pub content_type: String,
    pub url: String,
}
