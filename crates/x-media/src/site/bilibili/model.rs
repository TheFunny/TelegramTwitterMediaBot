//! Serde DTOs for the Bilibili dynamic detail endpoint
//! (`/x/polymer/web-dynamic/v1/detail`), mirroring live responses
//! (field paths verified 2026-09-17). Every field is optional so an API
//! shape change degrades to "no media" instead of a parse failure.

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub(crate) struct Detail {
    /// Business code: `0` = OK, `-352`/`-412` = risk control, `500`/`4101147`
    /// = gone.
    pub(crate) code: i64,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) data: Option<Data>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Data {
    #[serde(default)]
    pub(crate) item: Option<Box<Item>>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Item {
    /// The dynamic id, same numeric id as in the URL.
    #[serde(default)]
    pub(crate) id_str: String,
    #[serde(default)]
    pub(crate) modules: Option<Modules>,
    /// The quoted dynamic when this item is a forward. A forward shell often
    /// carries no media of its own — the original holds it.
    #[serde(default)]
    pub(crate) orig: Option<Box<Item>>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Modules {
    #[serde(default)]
    pub(crate) module_author: Option<Author>,
    #[serde(default)]
    pub(crate) module_dynamic: Option<Dynamic>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Author {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) mid: Option<i64>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Dynamic {
    #[serde(default)]
    pub(crate) desc: Option<Desc>,
    #[serde(default)]
    pub(crate) major: Option<Major>,
    /// A single topic (`{"id":…,"name":…}`), the dynamic's only tag source.
    #[serde(default)]
    pub(crate) topic: Option<Topic>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Desc {
    #[serde(default)]
    pub(crate) text: String,
}

/// `major` is a tagged union: `type` (`MAJOR_TYPE_DRAW` / `_ARCHIVE` / …)
/// plus one payload object per type. Only the two payloads this adapter reads
/// are modeled; an unknown major simply yields no media.
#[derive(Deserialize, Debug)]
pub(crate) struct Major {
    #[serde(default)]
    pub(crate) draw: Option<Draw>,
    #[serde(default)]
    pub(crate) archive: Option<Archive>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Draw {
    #[serde(default)]
    pub(crate) items: Vec<Pic>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Pic {
    /// Image URL, served as `http://` — normalized to https by the adapter.
    pub(crate) src: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Archive {
    /// The attached video's cover — the only image an AV dynamic has (the
    /// video itself is deliberately not resolved, see the module docs).
    #[serde(default)]
    pub(crate) cover: Option<String>,
    /// The video's title. An AV dynamic has no body of its own (`desc` comes
    /// back `null`), so this card title is the post's content.
    #[serde(default)]
    pub(crate) title: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Topic {
    #[serde(default)]
    pub(crate) name: String,
}

/// Response of the anonymous fingerprint endpoint (`/x/frontend/finger/spi`),
/// the source of the adapter's device cookies.
#[derive(Deserialize, Debug)]
pub(crate) struct Fingerprint {
    #[serde(default)]
    pub(crate) data: Option<FingerprintData>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct FingerprintData {
    /// Sent as the `buvid3` cookie.
    #[serde(default, rename = "b_3")]
    pub(crate) buvid3: String,
    /// Sent as the `buvid4` cookie.
    #[serde(default, rename = "b_4")]
    pub(crate) buvid4: String,
}
