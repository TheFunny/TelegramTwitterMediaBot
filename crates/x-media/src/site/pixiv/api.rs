//! Native pixiv app-API client (replaces pixiv3-rs).
//!
//! Token exchange against `oauth.secure.pixiv.net` and illust detail against
//! `app-api.pixiv.net`, deserialized with the kept `model.rs` types.

use super::interface::Illustration;
use super::model::{IllustrationModel, TypeModel, UgoiraMetadataModel};
use crate::media::Media;
use crate::site::FetchError;
use std::env;
use std::fmt;
use std::io::{Cursor, Read};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

const AUTH_TOKEN_URL: &str = "https://oauth.secure.pixiv.net/auth/token";
const APP_API_URL: &str = "https://app-api.pixiv.net";
const CLIENT_ID: &str = "MOBrBDS8blbauoSck0ZfDbtuzpyT";
const CLIENT_SECRET: &str = "lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj";
const AUTH_USER_AGENT: &str = "PixivAndroidApp/5.0.234 (Android 11; Pixel 5)";
const APP_USER_AGENT: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";
/// Token refresh safe margin (seconds).
const TOKEN_REFRESH_SAFE_MARGIN: u64 = 300;

#[derive(Debug)]
pub enum PixivError {
    /// No refresh token available (PIXIV_REFRESH_TOKEN unset).
    NoAuth,
    Http(reqwest::Error),
    Json(serde_json::Error),
    Api(String),
}

impl fmt::Display for PixivError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixivError::NoAuth => write!(f, "pixiv: no authentication"),
            PixivError::Http(e) => write!(f, "pixiv http error: {e}"),
            PixivError::Json(e) => write!(f, "pixiv json error: {e}"),
            PixivError::Api(message) => write!(f, "pixiv api error: {message}"),
        }
    }
}

impl std::error::Error for PixivError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PixivError::Http(e) => Some(e),
            PixivError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for PixivError {
    fn from(e: reqwest::Error) -> Self {
        PixivError::Http(e)
    }
}

impl From<serde_json::Error> for PixivError {
    fn from(e: serde_json::Error) -> Self {
        PixivError::Json(e)
    }
}

/// Native pixiv app-API client.
pub struct PixivAPI {
    refresh_token: String,
    access_token: tokio::sync::Mutex<Option<(String, SystemTime)>>,
}

impl PixivAPI {
    pub fn new(refresh_token: String) -> Self {
        Self {
            refresh_token,
            access_token: tokio::sync::Mutex::new(None),
        }
    }

    /// Returns a valid access token, exchanging the refresh token when none
    /// is cached or it has expired.
    pub async fn get_access_token(&self) -> Result<String, PixivError> {
        let mut guard = self.access_token.lock().await;
        if let Some((token, expires_at)) = guard.as_ref()
            && *expires_at > SystemTime::now()
        {
            return Ok(token.clone());
        }
        let response = crate::site::CLIENT
            .post(AUTH_TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("grant_type", "refresh_token"),
                ("include_policy", "true"),
                ("refresh_token", &self.refresh_token),
            ])
            .header("User-Agent", AUTH_USER_AGENT)
            .send()
            .await?;
        let json: serde_json::Value = serde_json::from_str(&response.text().await?)?;
        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let message = json
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("invalid token response");
                PixivError::Api(message.to_string())
            })?
            .to_string();
        let expires_in = json
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .filter(|&sec| sec > 0)
            .unwrap_or(3600);
        let expires_at = SystemTime::now()
            + Duration::from_secs(expires_in.saturating_sub(TOKEN_REFRESH_SAFE_MARGIN));
        *guard = Some((access_token.clone(), expires_at));
        Ok(access_token)
    }

    /// Fetches illust detail from the app API.
    pub async fn illust_detail(&self, illust_id: u64) -> Result<IllustrationModel, PixivError> {
        let access_token = self.get_access_token().await?;
        let response = crate::site::CLIENT
            .get(format!(
                "{APP_API_URL}/v1/illust/detail?illust_id={illust_id}"
            ))
            .header("app-os", "ios")
            .header("app-os-version", "14.6")
            .header("User-Agent", APP_USER_AGENT)
            .bearer_auth(access_token)
            .send()
            .await?;
        let json: serde_json::Value = serde_json::from_str(&response.text().await?)?;
        if json.get("error").is_some() {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("illust detail failed");
            return Err(PixivError::Api(message.to_string()));
        }
        let illust = json
            .get("illust")
            .ok_or_else(|| PixivError::Api("missing illust in response".to_string()))?;
        Ok(serde_json::from_value(illust.clone())?)
    }

    pub async fn fetch(&self, illust_id: u64) -> Result<Illustration, FetchError> {
        let model = self.illust_detail(illust_id).await?;
        let mut illustration = Illustration::from_model(&model);
        if matches!(&model.r#type, TypeModel::Ugoira) {
            // Real ugoira support: download the frame zip and encode an MP4.
            // Without ffmpeg (or on encode failure) the post stays
            // unsupported (empty media, like Python).
            match self.ugoira_video(illust_id).await {
                Ok(Some((mp4_path, _keep_alive))) => {
                    illustration.media.push(Media::Video {
                        title: None,
                        url: mp4_path,
                        thumbnail_url: model.image_urls.medium.clone(),
                    });
                    illustration._keep_alive = Some(_keep_alive);
                }
                Ok(None) => {}
                Err(e) => log::error!("ugoira encode failed for {illust_id}: {e}"),
            }
        }
        Ok(illustration)
    }

    /// Fetches ugoira metadata (frame zip + frame delays) from the app API.
    pub async fn ugoira_metadata(&self, illust_id: u64) -> Result<UgoiraMetadataModel, PixivError> {
        let access_token = self.get_access_token().await?;
        let response = crate::site::CLIENT
            .get(format!(
                "{APP_API_URL}/v1/ugoira/metadata?illust_id={illust_id}"
            ))
            .header("app-os", "ios")
            .header("app-os-version", "14.6")
            .header("User-Agent", APP_USER_AGENT)
            .bearer_auth(access_token)
            .send()
            .await?;
        let json: serde_json::Value = serde_json::from_str(&response.text().await?)?;
        if json.get("error").is_some() {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("ugoira metadata failed");
            return Err(PixivError::Api(message.to_string()));
        }
        let metadata = json
            .get("ugoira_metadata")
            .ok_or_else(|| PixivError::Api("missing ugoira_metadata".to_string()))?;
        Ok(serde_json::from_value(metadata.clone())?)
    }

    /// Downloads the frame zip and encodes one MP4 via ffmpeg. Returns the
    /// MP4 path plus the temp directory that must stay alive until the file
    /// is uploaded.
    async fn ugoira_video(
        &self,
        illust_id: u64,
    ) -> Result<Option<(String, tempfile::TempDir)>, PixivError> {
        if !crate::site::ffmpeg_available() {
            crate::site::log_once_ffmpeg_missing();
            return Ok(None);
        }
        let metadata = self.ugoira_metadata(illust_id).await?;
        if metadata.frames.is_empty() {
            return Ok(None);
        }
        let zip_url = metadata
            .zip_url
            .clone()
            .or_else(|| metadata.zip_urls.as_ref().map(|z| z.medium.clone()));
        let Some(zip_url) = zip_url else {
            return Ok(None);
        };
        let zip_bytes = crate::site::download_media(&zip_url)
            .await
            .map_err(|e| match e {
                FetchError::Http(e) => PixivError::Http(e),
                other => PixivError::Api(format!("frame zip download failed: {other}")),
            })?;
        let frame_delays = metadata.frames.iter().map(|f| f.delay).collect::<Vec<_>>();
        let result =
            tokio::task::spawn_blocking(move || -> Result<(String, tempfile::TempDir), String> {
                let frames_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
                let out_dir = tempfile::tempdir().map_err(|e| e.to_string())?;

                // Extract frames to canonical zero-padded names; pixiv ugoira
                // frames are uniformly jpg or png per artwork.
                let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
                    .map_err(|e| format!("unzip: {e}"))?;
                // pixiv ugoira frames are uniformly jpg or png per artwork; take
                // the extension from the first entry.
                let extension = if archive.len() > 0 {
                    let first_name = archive
                        .by_index(0)
                        .map_err(|e| e.to_string())?
                        .name()
                        .to_string();
                    first_name.rsplit('.').next().unwrap_or("jpg").to_string()
                } else {
                    "jpg".to_string()
                };
                let mut count = 0usize;
                for i in 0..archive.len() {
                    let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
                    let mut bytes = Vec::new();
                    entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                    let path = frames_dir
                        .path()
                        .join(format!("img_{count:05}.{extension}"));
                    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                    count += 1;
                }
                if count == 0 {
                    return Err("empty frame zip".to_string());
                }

                // Constant rate from the median frame delay (ms).
                let mut delays = frame_delays;
                delays.sort_unstable();
                let median = delays[delays.len() / 2].max(1);
                let framerate = 1000.0 / median as f64;

                let output = out_dir.path().join("ugoira.mp4");
                let status = std::process::Command::new("ffmpeg")
                    .args([
                        "-y",
                        "-framerate",
                        &framerate.to_string(),
                        "-i",
                        &frames_dir
                            .path()
                            .join(format!("img_%05d.{extension}"))
                            .to_string_lossy(),
                        // libx264 needs even dimensions; pixiv ugoira frames can
                        // be odd-sized (e.g. 277x405).
                        "-vf",
                        "scale=trunc(iw/2)*2:trunc(ih/2)*2",
                        "-c:v",
                        "libx264",
                        "-pix_fmt",
                        "yuv420p",
                        "-movflags",
                        "+faststart",
                        &output.to_string_lossy(),
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| format!("ffmpeg spawn failed: {e}"))?;
                if !status.success() {
                    return Err(format!("ffmpeg exited with {status}"));
                }
                Ok((output.to_string_lossy().into_owned(), out_dir))
            })
            .await
            .expect("ugoira encode worker panicked");
        match result {
            Ok(pair) => Ok(Some(pair)),
            Err(message) => {
                log::error!("ugoira encode failed for {illust_id}: {message}");
                Ok(None)
            }
        }
    }
}

/// pixiv3-rs replacement: `None` when `PIXIV_REFRESH_TOKEN` is unset.
static PIXIV_CLIENT: LazyLock<Option<PixivAPI>> =
    LazyLock::new(|| env::var("PIXIV_REFRESH_TOKEN").ok().map(PixivAPI::new));

/// Set at startup when the login validation fails; pixiv stays disabled until
/// the next process start.
static DISABLED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    !DISABLED.load(Ordering::Relaxed) && env::var("PIXIV_REFRESH_TOKEN").is_ok()
}

/// Permanently disables pixiv until the next process start.
pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// Forces the refresh-token → access-token exchange now, surfacing invalid
/// tokens and network errors. Called once at bot startup; on failure the bot
/// calls [`disable`].
pub async fn validate() -> Result<(), PixivError> {
    match PIXIV_CLIENT.as_ref() {
        None => Err(PixivError::NoAuth),
        Some(client) => {
            client.get_access_token().await?;
            Ok(())
        }
    }
}

pub async fn fetch(illust_id: u64) -> Result<Illustration, FetchError> {
    let client = PIXIV_CLIENT
        .as_ref()
        .filter(|_| enabled())
        .ok_or(FetchError::Pixiv(PixivError::NoAuth))?;
    client.fetch(illust_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotenv::dotenv;

    #[tokio::test]
    async fn test_fetch() {
        dotenv().ok();
        let result = fetch(126839080).await;
        assert!(result.is_ok());
        println!("{:#?}", result);
    }

    #[tokio::test]
    async fn validate_with_bogus_token_fails() {
        dotenv().ok();
        // A bogus token must surface as Api error (invalid_grant), not panic.
        let client = PixivAPI::new("bogus_token_for_testing".to_string());
        let result = client.get_access_token().await;
        assert!(matches!(result, Err(PixivError::Api(_))), "got {result:?}");
    }
}
