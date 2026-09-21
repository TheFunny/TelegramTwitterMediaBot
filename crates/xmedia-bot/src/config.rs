//! Central env handling. The only other places that read env are
//! `Bot::from_env` (TELOXIDE_TOKEN) and x-media (PIXIV_REFRESH_TOKEN,
//! TWITTER_AUTH_TOKEN, BILIBILI_COOKIE).

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
    /// CAPTION_QUOTE_TEXT_CHARS, default 200: a post whose text (title plus
    /// content) is at least this many characters gets that text wrapped in an
    /// expandable blockquote inside its caption. `0` disables the wrap.
    pub caption_quote_text_chars: usize,
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

        /// A setting that must parse when it is set: an unparseable value warns
        /// (naming the variable) and counts as unset.
        fn parse_opt<T: std::str::FromStr>(name: &str) -> Option<T> {
            env::var(name).ok().and_then(|s| {
                s.parse::<T>().ok().or_else(|| {
                    log::warn!("invalid {name}={s:?}");
                    None
                })
            })
        }

        let admin_ids = env::var("BOT_ADMIN")
            .map(|s| {
                let mut bad = Vec::new();
                let ids: Vec<i64> = s
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .filter_map(|part| match part.parse::<i64>() {
                        Ok(id) => Some(id),
                        Err(_) => {
                            bad.push(part);
                            None
                        }
                    })
                    .collect();
                if !bad.is_empty() {
                    log::warn!("BOT_ADMIN: ignoring non-numeric ids: {bad:?}");
                }
                ids
            })
            .unwrap_or_default();

        let edit_message_ttl =
            Duration::from_secs(parse_u64("EDIT_MESSAGE_TTL_SECONDS", 24 * 3600));
        let link_cache_ttl =
            Duration::from_secs(parse_u64("LINK_CACHE_TTL_SECONDS", 7 * 24 * 3600));
        let caption_quote_text_chars = parse_u64("CAPTION_QUOTE_TEXT_CHARS", 200) as usize;

        let webhook_enabled = env::var("WEBHOOK")
            .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1"));
        // The webhook settings are consumed by `.expect()` in main when
        // WEBHOOK=true, so an unparseable value fails fast at startup with a
        // clear message; still log here for the WEBHOOK=false case.
        let webhook_url = parse_opt::<url::Url>("WEBHOOK_URL");
        let webhook_listen = parse_opt::<IpAddr>("WEBHOOK_LISTEN");
        let webhook_port = parse_opt::<u16>("WEBHOOK_PORT");
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
            caption_quote_text_chars,
            webhook_enabled,
            webhook_url,
            webhook_listen,
            webhook_port,
            webhook_cert,
            webhook_secret_token,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids an operator's `BOT_ADMIN` yields: blanks dropped, non-numeric
    /// entries warned about and skipped, the rest kept in order. Parsed once —
    /// the split used to parse every entry twice.
    #[test]
    fn bot_admin_keeps_the_numeric_ids_in_order() {
        // SAFETY: no other test reads BOT_ADMIN, and the value is restored
        // before this test returns.
        let previous = env::var("BOT_ADMIN").ok();
        unsafe { env::set_var("BOT_ADMIN", " 7 ,abc,42, ,") };
        let ids = Config::load().admin_ids;
        match previous {
            Some(value) => unsafe { env::set_var("BOT_ADMIN", value) },
            None => unsafe { env::remove_var("BOT_ADMIN") },
        }
        assert_eq!(ids, vec![7, 42]);
    }
}
