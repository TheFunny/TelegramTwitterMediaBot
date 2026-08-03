use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub(crate) struct Info {
    pub(crate) thread: Thread,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "$type")]
pub(crate) enum Thread {
    #[serde(rename = "app.bsky.feed.defs#threadViewPost")]
    Post { post: Post },
    #[serde(rename = "app.bsky.feed.defs#notFoundPost")]
    NotFound,
    #[serde(rename = "app.bsky.feed.defs#blockedPost")]
    Blocked,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Post {
    pub(crate) author: Author,
    pub(crate) record: PostRecord,
    pub(crate) embed: Option<Media>,
    #[serde(default)]
    pub(crate) labels: Vec<Label>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Author {
    pub(crate) handle: String,
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct PostRecord {
    pub(crate) text: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "$type")]
pub(crate) enum Media {
    #[serde(rename = "app.bsky.embed.images#view")]
    Images { images: Vec<Image> },
    #[serde(rename = "app.bsky.embed.video#view")]
    Video { playlist: String, thumbnail: String },
    #[serde(rename = "app.bsky.embed.external#view")]
    External,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Image {
    pub(crate) thumb: String,
    pub(crate) fullsize: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Label {
    pub(crate) val: String,
}
