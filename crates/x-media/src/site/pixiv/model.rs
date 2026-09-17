// Model set for the native pixiv app-API client (app-api.pixiv.net).

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct IllustrationModel {
    pub id: u64,
    pub title: String,
    /// The artwork's description as the app API returns it — HTML in most
    /// works (`<br />`, `<a href>`, sometimes `<p>`), empty for many.
    #[serde(default)]
    pub caption: String,
    pub r#type: TypeModel,
    pub image_urls: ImageUrlsModel,
    pub user: UserInfoModel,
    pub tags: Vec<IllustrationTagModel>,
    pub page_count: u8,
    pub sanity_level: u8,
    /// 0 = undefined (unlabeled), 1 = not AI, 2 = AI-generated.
    pub illust_ai_type: i32,
    pub meta_single_page: MetaSinglePageModel,
    pub meta_pages: Vec<MetaPageModel>,
}

#[derive(Deserialize, Debug)]
pub enum TypeModel {
    #[serde(rename = "illust")]
    Illust,
    #[serde(rename = "manga")]
    Manga,
    #[serde(rename = "ugoira")]
    Ugoira,
}

#[derive(Deserialize, Debug)]
pub struct UserInfoModel {
    pub id: u64,
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct ImageUrlsModel {
    pub medium: String,
    pub large: String,
    #[serde(default)]
    pub original: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct IllustrationTagModel {
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct MetaSinglePageModel {
    #[serde(default)]
    pub original_image_url: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct MetaPageModel {
    pub image_urls: ImageUrlsModel,
}

#[derive(Deserialize, Debug)]
pub struct UgoiraMetadataModel {
    /// Older API shape (`zip_url`); newer responses use `zip_urls.medium`.
    #[serde(default)]
    pub zip_url: Option<String>,
    #[serde(default)]
    pub zip_urls: Option<UgoiraZipUrlsModel>,
    pub frames: Vec<UgoiraFrameModel>,
}

#[derive(Deserialize, Debug)]
pub struct UgoiraZipUrlsModel {
    pub medium: String,
}

#[derive(Deserialize, Debug)]
pub struct UgoiraFrameModel {
    pub delay: u32,
}
