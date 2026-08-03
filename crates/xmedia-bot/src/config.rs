//! Central env handling. The only other places that read env are
//! `Bot::from_env` (TELOXIDE_TOKEN) and x-media (PIXIV_REFRESH_TOKEN).

use std::env;
use std::net::IpAddr;
use std::time::Duration;

pub struct Config {
    /// BOT_ADMIN: comma-separated ints; empty when unset.
    pub admin_ids: Vec<i64>,
    /// EDIT_MESSAGE_TTL_SECONDS, default 86400 (24h).
    pub edit_message_ttl: Duration,
    // Webhook settings (moved out of main; names/defaults unchanged).
    pub webhook_enabled: bool,
    pub webhook_url: Option<url::Url>,
    pub webhook_listen: Option<IpAddr>,
    pub webhook_port: Option<u16>,
    pub webhook_cert: Option<String>,
    pub webhook_secret_token: Option<String>,
}

impl Config {
    pub fn load() -> Config {
        let admin_ids = env::var("BOT_ADMIN")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|part| part.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default();

    let edit_message_ttl = env::var("EDIT_MESSAGE_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(86400));

    let webhook_enabled = env::var("WEBHOOK")
        .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1"));
    let webhook_url = env::var("WEBHOOK_URL").ok().and_then(|s| s.parse().ok());
    let webhook_listen = env::var("WEBHOOK_LISTEN").ok().and_then(|s| s.parse().ok());
    let webhook_port = env::var("WEBHOOK_PORT").ok().and_then(|s| s.parse().ok());
    let webhook_cert = env::var("WEBHOOK_CERT").ok();
    let webhook_secret_token = env::var("WEBHOOK_SECRET_TOKEN").ok();

        Config {
            admin_ids,
            edit_message_ttl,
            webhook_enabled,
            webhook_url,
            webhook_listen,
            webhook_port,
            webhook_cert,
            webhook_secret_token,
        }
    }
}
