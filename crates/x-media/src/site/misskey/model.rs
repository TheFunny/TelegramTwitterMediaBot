use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub(crate) struct Note {
    pub(crate) id: String,
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) cw: Option<String>,
    pub(crate) user: User,
    #[serde(default)]
    pub(crate) files: Vec<DriveFile>,
    /// Embedded original note when this note is a renote; the shell's own
    /// text/files are usually empty and the content lives here.
    #[serde(default)]
    pub(crate) renote: Option<Box<Note>>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct User {
    pub(crate) name: Option<String>,
    pub(crate) username: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct DriveFile {
    #[serde(rename = "type")]
    pub(crate) mime_type: String,
    pub(crate) url: String,
    #[serde(default, rename = "thumbnailUrl")]
    pub(crate) thumbnail_url: Option<String>,
    #[serde(default, rename = "isSensitive")]
    pub(crate) is_sensitive: bool,
}
