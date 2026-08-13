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
    /// LINK_CACHE_TTL_SECONDS, default 604800 (7 days).
    pub link_cache_ttl: Duration,
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
        // Fail-fast helpers: a misspelled value must not silently fall back
        // to a default and run with different behavior than the operator
        // intended — log a loud warning naming the variable instead.
        fn parse_u64(name: &str, default: u64) -> u64 {
            match env::var(name) {
                Ok(v) => v.parse::<u64>().unwrap_or_else(|_| {
                    log::warn!("invalid {name}={v:?}; using default {default}");
                    default
                }),
                Err(_) => default,
            }
        }

        let admin_ids = match env::var("BOT_ADMIN") {
            Ok(s) => {
                let (ids, bad): (Vec<_>, Vec<_>) = s
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .partition(|part| part.parse::<i64>().is_ok());
                if !bad.is_empty() {
                    log::warn!("BOT_ADMIN: ignoring non-numeric ids: {bad:?}");
                }
                ids.into_iter()
                    .filter_map(|p| p.parse::<i64>().ok())
                    .collect()
            }
            Err(_) => Vec::new(),
        };

        let edit_message_ttl =
            Duration::from_secs(parse_u64("EDIT_MESSAGE_TTL_SECONDS", 24 * 3600));
        let link_cache_ttl =
            Duration::from_secs(parse_u64("LINK_CACHE_TTL_SECONDS", 7 * 24 * 3600));

        let webhook_enabled = env::var("WEBHOOK")
            .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1"));
        // The webhook settings are consumed by `.expect()` in main when
        // WEBHOOK=true, so an unparseable value fails fast at startup with a
        // clear message; still log here for the WEBHOOK=false case.
        let webhook_url = env::var("WEBHOOK_URL").ok().and_then(|s| {
            s.parse::<url::Url>().ok().or_else(|| {
                log::warn!("invalid WEBHOOK_URL={s:?}");
                None
            })
        });
        let webhook_listen = env::var("WEBHOOK_LISTEN").ok().and_then(|s| {
            s.parse::<IpAddr>().ok().or_else(|| {
                log::warn!("invalid WEBHOOK_LISTEN={s:?}");
                None
            })
        });
        let webhook_port = env::var("WEBHOOK_PORT").ok().and_then(|s| {
            s.parse::<u16>().ok().or_else(|| {
                log::warn!("invalid WEBHOOK_PORT={s:?}");
                None
            })
        });
        // Empty strings count as unset (e.g. `-e WEBHOOK_CERT=` to disable a
        // value that would otherwise come from `.env`).
        let webhook_cert = env::var("WEBHOOK_CERT").ok().filter(|s| !s.is_empty());
        let webhook_secret_token = env::var("WEBHOOK_SECRET_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());

        Config {
            admin_ids,
            edit_message_ttl,
            link_cache_ttl,
            webhook_enabled,
            webhook_url,
            webhook_listen,
            webhook_port,
            webhook_cert,
            webhook_secret_token,
        }
    }
}
