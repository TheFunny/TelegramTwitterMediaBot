//! Native pixiv app-API client (replaces pixiv3-rs).
//!
//! Token exchange against `oauth.secure.pixiv.net` and illust detail against
//! `app-api.pixiv.net`, deserialized with the kept `model.rs` types.

use super::interface::Illustration;
use super::model::{IllustrationModel, UgoiraMetadataModel};
use crate::media::Media;
use crate::site::FetchError;
use std::env;
use std::io::Read;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use thiserror::Error;

const AUTH_TOKEN_URL: &str = "https://oauth.secure.pixiv.net/auth/token";
const APP_API_URL: &str = "https://app-api.pixiv.net";
const CLIENT_ID: &str = "MOBrBDS8blbauoSck0ZfDbtuzpyT";
const CLIENT_SECRET: &str = "lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj";
const AUTH_USER_AGENT: &str = "PixivAndroidApp/5.0.234 (Android 11; Pixel 5)";
const APP_USER_AGENT: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";
/// Token refresh safe margin (seconds).
const TOKEN_REFRESH_SAFE_MARGIN: u64 = 300;

#[derive(Debug, Error)]
pub enum PixivError {
    /// No refresh token available (PIXIV_REFRESH_TOKEN unset).
    #[error("pixiv: no authentication")]
    NoAuth,
    #[error("pixiv http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("pixiv json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Non-2xx HTTP status from the app API. The code lets [`crate::site::fetch`]
    /// retry only transient classes (429 / 5xx) instead of burning attempts on
    /// permanent 4xx (bad token, forbidden, not found).
    #[error("pixiv status {0}")]
    Status(u16),
    #[error("pixiv api error: {0}")]
    Api(String),
    /// A bad moment while preparing media: a transient download status
    /// (429 / 5xx), a stalled transfer or a temp-file write failure. A retry
    /// can change the answer, so the pixiv retry policy re-fetches these.
    #[error("transient pixiv error: {0}")]
    Transient(String),
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
        // Check the status *before* reading the body: a 429/5xx from the
        // token endpoint is worth retrying (the class comes from
        // `is_retryable`), while parsing a maintenance page as JSON turned it
        // into a permanent `Api`/`Json` error with no retry at all.
        if !response.status().is_success() {
            return Err(PixivError::Status(response.status().as_u16()));
        }
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
        if !response.status().is_success() {
            return Err(PixivError::Status(response.status().as_u16()));
        }
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
        if model.r#type == "ugoira" {
            // Real ugoira support: download the frame zip and encode an MP4.
            // Without ffmpeg the post stays unsupported (empty media, like
            // Python) — but a *failed* download/encode is reported instead:
            // a ugoira post has no static image to fall back to, so
            // swallowing it would present a transient zip-download error as
            // "this post has no media", with the retries skipped.
            match self.ugoira_video(illust_id).await {
                Ok(Some((mp4_path, _keep_alive))) => {
                    illustration.media.push(Media::Video {
                        url: mp4_path,
                        thumbnail_url: model.image_urls.medium.clone(),
                    });
                    illustration._keep_alive = Some(std::sync::Arc::new(_keep_alive));
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!("ugoira encode failed for {illust_id}: {e}");
                    return Err(FetchError::Pixiv(e));
                }
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
        if !response.status().is_success() {
            return Err(PixivError::Status(response.status().as_u16()));
        }
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
        // Stream the frame zip to a temp file instead of buffering it in
        // memory: ugoira zips can be hundreds of MB, and the old
        // download_media_limited path spiked RAM up to the size cap.
        let zip_file = tempfile::Builder::new()
            .prefix(crate::TEMP_FILE_PREFIX)
            .suffix(".zip")
            .tempfile()
            .map_err(|e| PixivError::Api(format!("temp zip failed: {e}")))?;
        // Stream through a tokio handle: a sync write per chunk would stall
        // an executor thread for the whole (up to 512 MiB) download. The
        // clone shares the file offset with `zip_file`, so the extraction
        // below reads what was written, and dropping it after the download
        // hands every byte to the OS.
        let mut zip_out = tokio::fs::File::from_std(
            zip_file
                .as_file()
                .try_clone()
                .map_err(|e| PixivError::Api(format!("temp zip clone failed: {e}")))?,
        );
        crate::site::download_media_to_file(&zip_url, 512 * 1024 * 1024, &mut zip_out)
            .await
            .map_err(|e| match e {
                FetchError::Http(e) => PixivError::Http(e),
                // A bad moment (429/5xx, a stalled transfer, a temp-file
                // write failure) must stay retryable: folding it into Api
                // made one hiccup permanently fail the whole ugoira post,
                // while the bot's own upload downloads retry the same
                // classes.
                transient @ (FetchError::Transient(_) | FetchError::Io(_)) => {
                    PixivError::Transient(format!("frame zip download failed: {transient}"))
                }
                other => PixivError::Api(format!("frame zip download failed: {other}")),
            })?;
        drop(zip_out);
        let frame_delays = metadata.frames.iter().map(|f| f.delay).collect::<Vec<_>>();
        let result =
            tokio::task::spawn_blocking(move || -> Result<(String, tempfile::TempDir), String> {
                let frames_dir = tempfile::Builder::new()
                    .prefix(crate::TEMP_FILE_PREFIX)
                    .tempdir()
                    .map_err(|e| e.to_string())?;
                let out_dir = tempfile::Builder::new()
                    .prefix(crate::TEMP_FILE_PREFIX)
                    .tempdir()
                    .map_err(|e| e.to_string())?;

                // Extract frames to canonical zero-padded names; pixiv ugoira
                // frames are uniformly jpg or png per artwork. The zip is read
                // from disk; `zip_file` stays alive for the whole extraction.
                let mut archive = zip::ZipArchive::new(
                    std::fs::File::open(zip_file.path()).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("unzip: {e}"))?;
                if archive.is_empty() {
                    return Err("empty frame zip".to_string());
                }
                // Uniform jpg or png per artwork; sniff the first entry's
                // magic bytes instead of trusting its filename.
                let first = archive.by_index(0).map_err(|e| e.to_string())?;
                let mut first_bytes = Vec::new();
                first
                    .take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut first_bytes)
                    .map_err(|e| e.to_string())?;
                if first_bytes.len() > 64 * 1024 * 1024 {
                    return Err("frame exceeds size cap".to_string());
                }
                let extension = if first_bytes.starts_with(&[0xFF, 0xD8]) {
                    "jpg"
                } else if first_bytes.starts_with(b"\x89PNG") {
                    "png"
                } else {
                    "jpg"
                };
                let mut count = 0usize;
                {
                    let path = frames_dir
                        .path()
                        .join(format!("img_{count:05}.{extension}"));
                    std::fs::write(&path, &first_bytes).map_err(|e| e.to_string())?;
                    count += 1;
                }
                for i in 1..archive.len() {
                    let entry = archive.by_index(i).map_err(|e| e.to_string())?;
                    if entry.size() > 64 * 1024 * 1024 {
                        return Err(format!("frame {i} exceeds size cap"));
                    }
                    let mut bytes = Vec::new();
                    entry
                        .take(64 * 1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    if bytes.len() > 64 * 1024 * 1024 {
                        return Err(format!("frame {i} exceeds size cap"));
                    }
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
            .map_err(|e| {
                log::error!("ugoira encode worker panicked for {illust_id}: {e}");
                PixivError::Api(format!("ugoira worker failed: {e}"))
            })?;
        match result {
            Ok(pair) => Ok(Some(pair)),
            Err(message) => {
                log::error!("ugoira encode failed for {illust_id}: {message}");
                Ok(None)
            }
        }
    }
}

/// pixiv3-rs replacement: `None` when `PIXIV_REFRESH_TOKEN` is unset or empty
/// (compose injects an empty string for a blank `.env` value; an empty token
/// must mean "not configured" instead of being sent to OAuth).
static PIXIV_CLIENT: LazyLock<Option<PixivAPI>> = LazyLock::new(|| {
    env::var("PIXIV_REFRESH_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .map(PixivAPI::new)
});

/// Set at startup when the login validation fails; pixiv stays disabled until
/// the next process start.
static DISABLED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    !DISABLED.load(Ordering::Relaxed)
        && env::var("PIXIV_REFRESH_TOKEN").is_ok_and(|token| !token.is_empty())
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

    /// An empty `PIXIV_REFRESH_TOKEN` (what compose injects for a blank
    /// `.env` value, and what an unset GitHub secret looks like) must read as
    /// "not configured", exactly like unset — otherwise a default deployment
    /// sends an empty refresh token to OAuth and fails login validation on
    /// every boot.
    #[test]
    fn empty_refresh_token_reads_as_unset() {
        // SAFETY: the value is restored before returning; `enabled()` keys on
        // this variable alone and no other test mutates it. Concurrent readers
        // see unset or empty, which this very fix makes the same answer.
        let previous = env::var("PIXIV_REFRESH_TOKEN").ok();
        unsafe { env::set_var("PIXIV_REFRESH_TOKEN", "") };
        let empty = enabled();
        unsafe { env::remove_var("PIXIV_REFRESH_TOKEN") };
        let unset = enabled();
        match previous {
            Some(value) => unsafe { env::set_var("PIXIV_REFRESH_TOKEN", value) },
            None => unsafe { env::remove_var("PIXIV_REFRESH_TOKEN") },
        }
        assert!(!empty, "an empty token must not enable pixiv");
        assert_eq!(empty, unset, "empty must read exactly like unset");
    }

    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to oauth.secure.pixiv.net"]
    async fn live_validate_with_bogus_token_fails() {
        dotenv().ok();
        // A rejected credential must surface as a permanent status, not a panic
        // and not a retryable class: the exchange answers 4xx and the status is
        // checked before the body is read (api.rs, `get_access_token`). This
        // used to assert `Api`, which that check made unreachable — `Api` is
        // only reached from a 2xx body without an `access_token`.
        let client = PixivAPI::new("bogus_token_for_testing".to_string());
        let result = client.get_access_token().await;
        assert!(
            matches!(result, Err(PixivError::Status(code)) if (400..500).contains(&code)),
            "got {result:?}"
        );
    }
}
