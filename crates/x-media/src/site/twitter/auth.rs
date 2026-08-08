//! Authenticated fallback for tweets the public syndication endpoint refuses
//! to serve (NSFW / age-restricted tweets come back as an empty `{}`).
//!
//! Mirrors nazurin's web API client ([`web.py`]) and is used *only* when
//! syndication reports [`FetchError::Sensitive`]: the private GraphQL
//! `TweetDetail` endpoint, authenticated with a browser session cookie from
//! `TWITTER_AUTH_TOKEN` (the `auth_token` cookie value of a logged-in x.com
//! session). A fresh random `ct0` is generated per call; X checks that the
//! `x-csrf-token` header matches the cookie, not that it issued the value.
//!
//! [`web.py`]: https://github.com/y-young/nazurin/blob/master/nazurin/sites/twitter/api/web.py
//!
//! # Caveats
//! - X rotates the GraphQL query id when it rolls the web app; if requests
//!   start failing, update [`TWEET_DETAIL_QUERY_ID`]. Fresh references from
//!   the actively maintained FxEmbed/FxEmbed: TweetDetail
//!   `R9IzzyzQBV87-DOWpcvDmw`, TweetResultByRestId `f2sagi1jweVHFkTUIHzmMQ`
//!   (the latter is anonymous and surfaces NSFW tweets as
//!   `reason: NsfwLoggedOut`).
//! - `x-client-transaction-id` is only required for `SearchTimeline`
//!   (verified against FxEmbed's `proxy/allowlist.ts`) — TweetDetail works
//!   without it; no need for the nazurin home-page/JS-bundle derivation.

use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::site::FetchError;

use super::interface::Tweet;

/// `auth_token` cookie of a logged-in x.com session; enables the fallback.
/// Trimmed: a CRLF `.env` (Windows) leaves a trailing `\r` on the value,
/// which would make the Cookie header invalid.
static AUTH_TOKEN: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("TWITTER_AUTH_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
});

/// Public "logged in" client token used by the x.com web app.
const LOGGED_IN_BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

/// `TweetDetail` query id (from nazurin; still valid as of 2026-08,
/// corroborated by the current FxEmbed build — see module caveats).
const TWEET_DETAIL_QUERY_ID: &str = "_8aYOgEDz35BrBcBal1-_w";

fn variables(id: &str) -> Value {
    json!({
        "focalTweetId": id,
        "with_rux_injections": false,
        "includePromotedContent": false,
        "withCommunity": true,
        "withQuickPromoteEligibilityTweetFields": false,
        "withBirdwatchNotes": false,
        "withVoice": true,
    })
}

fn features() -> Value {
    json!({
        "rweb_video_screen_enabled": false,
        "profile_label_improvements_pcf_label_in_post_enabled": true,
        "rweb_tipjar_consumption_enabled": true,
        "verified_phone_label_enabled": false,
        "creator_subscriptions_tweet_preview_api_enabled": true,
        "responsive_web_graphql_timeline_navigation_enabled": true,
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
        "premium_content_api_read_enabled": false,
        "communities_web_enable_tweet_community_results_fetch": true,
        "c9s_tweet_anatomy_moderator_badge_enabled": true,
        "responsive_web_grok_analyze_button_fetch_trends_enabled": false,
        "responsive_web_grok_analyze_post_followups_enabled": true,
        "responsive_web_jetfuel_frame": false,
        "responsive_web_grok_share_attachment_enabled": true,
        "articles_preview_enabled": true,
        "responsive_web_edit_tweet_api_enabled": true,
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
        "view_counts_everywhere_api_enabled": true,
        "longform_notetweets_consumption_enabled": true,
        "responsive_web_twitter_article_tweet_consumption_enabled": true,
        "tweet_awards_web_tipping_enabled": false,
        "responsive_web_grok_show_grok_translated_post": false,
        "responsive_web_grok_analysis_button_from_backend": true,
        "creator_subscriptions_quote_tweet_preview_enabled": false,
        "freedom_of_speech_not_reach_fetch_enabled": true,
        "standardized_nudges_misinfo": true,
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
        "longform_notetweets_rich_text_read_enabled": true,
        "longform_notetweets_inline_media_enabled": true,
        "responsive_web_grok_image_annotation_enabled": true,
        "responsive_web_enhance_cards_enabled": false,
    })
}

/// Whether the authenticated fallback is available.
pub fn enabled() -> bool {
    AUTH_TOKEN.is_some()
}

/// Fetches a tweet as the logged-in user via the private GraphQL API.
/// Returns the syndication-shaped [`Tweet`] (media included for NSFW posts).
pub async fn fetch(id: &str) -> Result<Tweet, FetchError> {
    let token = AUTH_TOKEN.as_deref().ok_or(FetchError::Sensitive)?;
    // 16 random bytes as 32 hex chars: X rejects ct0 values of any other
    // length with 403 code 353 ("matching csrf cookie and header").
    let ct0: String = (0..16)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();

    let response = crate::site::CLIENT
        .get(format!(
            "https://x.com/i/api/graphql/{TWEET_DETAIL_QUERY_ID}/TweetDetail"
        ))
        .query(&[
            ("variables", variables(id).to_string()),
            ("features", features().to_string()),
        ])
        .header("authorization", LOGGED_IN_BEARER)
        .header("x-csrf-token", &ct0)
        .header("x-twitter-auth-type", "OAuth2Session")
        .header("cookie", format!("auth_token={token}; ct0={ct0}"))
        .header("x-twitter-client-language", "en")
        .header("x-twitter-active-user", "yes")
        .header("referer", "https://x.com/")
        .send()
        .await?;
    // 404/410 = gone (permanent); 429/5xx = transient and retried by fetch.
    let status = response.status();
    if !status.is_success() {
        log::warn!("twitter auth fetch {id}: HTTP {status}");
        return match status.as_u16() {
            404 | 410 => Err(FetchError::NotFound),
            _ => Err(FetchError::Transient(format!(
                "twitter auth status {status}"
            ))),
        };
    }
    let text = response.text().await?;
    let json: Value = serde_json::from_str(&text)?;
    let result = parse_tweet_result(&json, id)?;
    let syndication_shape = to_syndication_shape(&result).ok_or_else(|| {
        FetchError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing tweet fields in GraphQL response",
        )))
    })?;
    Tweet::from_syndication_json(&syndication_shape.to_string()).map_err(FetchError::Json)
}

/// Locates the tweet for `id` in a `TweetDetail` response and unwraps
/// visibility wrappers / retweets, mirroring nazurin's `_process_response`.
fn parse_tweet_result(json: &Value, id: &str) -> Result<Value, FetchError> {
    if let Some(errors) = json.get("errors").and_then(|e| e.as_array()) {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect();
        log::warn!("twitter auth fetch {id} failed: {}", messages.join("; "));
        return Err(FetchError::NotFound);
    }

    let instructions = json
        .pointer("/data/threaded_conversation_with_injections_v2/instructions")
        .and_then(|v| v.as_array())
        .ok_or(FetchError::NotFound)?;
    for instruction in instructions {
        if instruction.get("type").and_then(|t| t.as_str()) != Some("TimelineAddEntries") {
            continue;
        }
        let entries = instruction
            .get("entries")
            .and_then(|e| e.as_array())
            .ok_or(FetchError::NotFound)?;
        let wanted = format!("tweet-{id}");
        for entry in entries {
            if entry.get("entryId").and_then(|i| i.as_str()) == Some(wanted.as_str()) {
                let result = entry
                    .pointer("/content/itemContent/tweet_results/result")
                    .ok_or(FetchError::NotFound)?;
                return normalize_tweet_result(result);
            }
        }
    }
    Err(FetchError::NotFound)
}

/// Unwraps TweetTombstone/TweetUnavailable errors, the
/// TweetWithVisibilityResults wrapper and retweets, returning the
/// `{core, legacy, ...}` tweet object.
fn normalize_tweet_result(result: &Value) -> Result<Value, FetchError> {
    match result.get("__typename").and_then(|t| t.as_str()) {
        Some("TweetTombstone") => {
            let text = result
                .pointer("/tombstone/text/text")
                .and_then(|t| t.as_str())
                .unwrap_or("tweet is unavailable");
            log::warn!("twitter auth fetch: tombstone: {text}");
            return Err(FetchError::NotFound);
        }
        Some("TweetUnavailable") => {
            let reason = result
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            log::warn!("twitter auth fetch: tweet unavailable: {reason}");
            return Err(FetchError::NotFound);
        }
        _ => {}
    }

    // TweetWithVisibilityResults (e.g. limited replies) nests the real tweet.
    let tweet = result.get("tweet").unwrap_or(result);
    // A retweet's media lives on the original tweet.
    if let Some(original) = tweet.pointer("/legacy/retweeted_status_result/result") {
        return Ok(original.clone());
    }
    Ok(tweet.clone())
}

/// Maps a GraphQL `{core, legacy, ...}` tweet onto the syndication JSON
/// shape [`Tweet::from_syndication_json`] parses, so the existing text /
/// media handling (t.co expansion, `name=orig`, mp4 variant) is reused.
fn to_syndication_shape(tweet: &Value) -> Option<Value> {
    let legacy = tweet.get("legacy")?;
    let user = tweet.pointer("/core/user_results/result/legacy")?;
    Some(json!({
        "id_str": legacy.get("id_str"),
        "text": legacy.get("full_text"),
        "user": {
            "name": user.get("name"),
            "screen_name": user.get("screen_name"),
        },
        "possibly_sensitive": legacy.get("possibly_sensitive"),
        "entities": legacy.get("entities"),
        "mediaDetails": legacy.pointer("/extended_entities/media"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tweet_result() -> Value {
        json!({
            "__typename": "Tweet",
            "core": {
                "user_results": {
                    "result": {
                        "legacy": { "name": "Display Name", "screen_name": "nsfw_author" }
                    }
                }
            },
            "legacy": {
                "id_str": "2083868672721039569",
                "full_text": "nsfw content https://t.co/abc123",
                "possibly_sensitive": true,
                "entities": {
                    // The appended media link lives in extended_entities.media,
                    // not entities.urls, so it has no expansion mapping and the
                    // content-based strip removes it.
                    "urls": []
                },
                "extended_entities": {
                    "media": [
                        {
                            "type": "photo",
                            "media_url_https": "https://pbs.twimg.com/media/nsfw.jpg",
                            "original_info": { "width": 1200, "height": 800 }
                        },
                        {
                            "type": "video",
                            "media_url_https": "https://pbs.twimg.com/thumb.jpg",
                            "video_info": {
                                "variants": [
                                    { "content_type": "application/x-mpegURL", "url": "https://x.com/pl.m3u8" },
                                    { "content_type": "video/mp4", "url": "https://video.twimg.com/nsfw.mp4" }
                                ]
                            }
                        }
                    ]
                }
            }
        })
    }

    fn conversation(tweet: Value) -> Value {
        json!({
            "data": {
                "threaded_conversation_with_injections_v2": {
                    "instructions": [
                        { "type": "TimelineAddEntries", "entries": [
                            { "entryId": "tweet-2083868672721039569",
                              "content": { "itemContent": { "tweet_results": { "result": tweet } } } }
                        ]}
                    ]
                }
            }
        })
    }

    #[test]
    fn parses_graphql_tweet_into_fetched() {
        let json = conversation(tweet_result());
        let result = parse_tweet_result(&json, "2083868672721039569").unwrap();
        let shape = to_syndication_shape(&result).unwrap();
        let tweet = Tweet::from_syndication_json(&shape.to_string()).unwrap();
        let fetched: crate::site::Fetched = tweet.into();

        assert!(fetched.sensitive);
        assert_eq!(fetched.media.len(), 2);
        match &fetched.media[0] {
            crate::media::Media::Illustration { url, .. } => {
                assert_eq!(url, "https://pbs.twimg.com/media/nsfw.jpg?name=orig");
            }
            other => panic!("expected illustration, got {other:?}"),
        }
        match &fetched.media[1] {
            crate::media::Media::Video { url, .. } => {
                assert_eq!(url, "https://video.twimg.com/nsfw.mp4");
            }
            other => panic!("expected video, got {other:?}"),
        }
        assert_eq!(
            fetched.source_url,
            "https://x.com/nsfw_author/status/2083868672721039569"
        );
        // The appended media short link (no URL-entity mapping) is stripped.
        assert_eq!(fetched.title, "nsfw content");
    }

    #[test]
    fn unwraps_retweet_to_original() {
        let original = tweet_result();
        let mut rt = tweet_result();
        rt["legacy"]["retweeted_status_result"] = json!({ "result": original });
        let json = conversation(rt);
        let result = parse_tweet_result(&json, "2083868672721039569").unwrap();
        assert!(result.pointer("/legacy/retweeted_status_result").is_none());
        assert_eq!(
            result.pointer("/legacy/id_str").unwrap(),
            "2083868672721039569"
        );
    }

    #[test]
    fn error_response_maps_to_not_found() {
        let json = json!({ "errors": [{ "message": "NsfwLoggedOut" }] });
        assert!(matches!(
            parse_tweet_result(&json, "1"),
            Err(FetchError::NotFound)
        ));
    }

    #[test]
    fn missing_entry_maps_to_not_found() {
        let json = conversation(json!({ "__typename": "Tweet" }));
        assert!(matches!(
            parse_tweet_result(&json, "999"),
            Err(FetchError::NotFound)
        ));
    }

    #[test]
    fn tombstone_maps_to_not_found() {
        let tombstone = json!({
            "__typename": "TweetTombstone",
            "tombstone": { "text": { "text": "Age-restricted adult content" } }
        });
        let json = conversation(tombstone);
        assert!(matches!(
            parse_tweet_result(&json, "2083868672721039569"),
            Err(FetchError::NotFound)
        ));
    }

    #[test]
    fn visibility_wrapper_unwraps() {
        let inner = tweet_result();
        let wrapped = json!({ "__typename": "TweetWithVisibilityResults", "tweet": inner });
        let json = conversation(wrapped);
        let result = parse_tweet_result(&json, "2083868672721039569").unwrap();
        assert_eq!(result.get("__typename").unwrap(), "Tweet");
    }
}
