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
                    log::warn!("invalid {name}; using default {default}");
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
                    log::warn!("invalid {name}");
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
                    log::warn!("BOT_ADMIN: ignoring {} non-numeric id(s)", bad.len());
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

    /// The webhook truth table: `Config::load` enables webhook mode only for
    /// a case-insensitive `true|yes|1`, and everything else — including the
    /// classic misspelling "on", which an operator would expect to work — is
    /// polling. Without this pin a typo silently ran a different transport
    /// (with P0's fail-fast secret check, or with no listener at all).
    #[test]
    fn webhook_flag_is_a_case_insensitive_truth_table() {
        // SAFETY: no other test *mutates* WEBHOOK, the value is restored
        // before this test returns, and concurrent Config::load callers in
        // other tests assert fields other than webhook_enabled.
        let previous = env::var("WEBHOOK").ok();
        for (value, expected) in [
            ("true", true),
            ("TRUE", true),
            ("Yes", true),
            ("1", true),
            ("false", false),
            ("on", false),
            ("", false),
        ] {
            unsafe { env::set_var("WEBHOOK", value) };
            assert_eq!(
                Config::load().webhook_enabled,
                expected,
                "WEBHOOK={value:?}"
            );
        }
        match previous {
            Some(value) => unsafe { env::set_var("WEBHOOK", value) },
            None => unsafe { env::remove_var("WEBHOOK") },
        }
    }

    /// A malformed TTL warns and falls back to the default instead of being
    /// parsed as 0 — the difference between a 24h edit-prompt expiry and a
    /// prompt that expires instantly, which an operator would only notice
    /// when the buttons stop working.
    #[test]
    fn invalid_ttl_falls_back_to_the_default() {
        // SAFETY: no other test *mutates* EDIT_MESSAGE_TTL_SECONDS; restored
        // below, and no other test asserts the TTL field.
        let previous = env::var("EDIT_MESSAGE_TTL_SECONDS").ok();
        unsafe { env::set_var("EDIT_MESSAGE_TTL_SECONDS", "not-a-number") };
        let invalid = Config::load().edit_message_ttl;
        unsafe { env::set_var("EDIT_MESSAGE_TTL_SECONDS", "120") };
        let valid = Config::load().edit_message_ttl;
        match previous {
            Some(value) => unsafe { env::set_var("EDIT_MESSAGE_TTL_SECONDS", value) },
            None => unsafe { env::remove_var("EDIT_MESSAGE_TTL_SECONDS") },
        }
        assert_eq!(
            invalid,
            Duration::from_secs(24 * 3600),
            "an unparseable value falls back to the default"
        );
        assert_eq!(
            valid,
            Duration::from_secs(120),
            "a valid value is taken as-is"
        );
    }

    /// A blank WEBHOOK_CERT / WEBHOOK_SECRET_TOKEN counts as unset — compose
    /// injects `${VAR:-}` as an empty string for a commented-out template
    /// line — while a present value is kept (the `-e VAR=` disable idiom).
    #[test]
    fn blank_webhook_cert_and_secret_count_as_unset() {
        // SAFETY: no other test *mutates* these two, both are restored
        // below, and no other test asserts them.
        let prev_cert = env::var("WEBHOOK_CERT").ok();
        let prev_secret = env::var("WEBHOOK_SECRET_TOKEN").ok();
        unsafe { env::set_var("WEBHOOK_CERT", "") };
        unsafe { env::set_var("WEBHOOK_SECRET_TOKEN", "") };
        let blank = Config::load();
        unsafe { env::set_var("WEBHOOK_CERT", "/x/cert.pem") };
        unsafe { env::set_var("WEBHOOK_SECRET_TOKEN", "s3cret") };
        let present = Config::load();
        match prev_cert {
            Some(value) => unsafe { env::set_var("WEBHOOK_CERT", value) },
            None => unsafe { env::remove_var("WEBHOOK_CERT") },
        }
        match prev_secret {
            Some(value) => unsafe { env::set_var("WEBHOOK_SECRET_TOKEN", value) },
            None => unsafe { env::remove_var("WEBHOOK_SECRET_TOKEN") },
        }
        assert_eq!(blank.webhook_cert, None, "blank must read as unset");
        assert_eq!(blank.webhook_secret_token, None, "blank must read as unset");
        assert_eq!(present.webhook_cert.as_deref(), Some("/x/cert.pem"));
        assert_eq!(present.webhook_secret_token.as_deref(), Some("s3cret"));
    }
}
