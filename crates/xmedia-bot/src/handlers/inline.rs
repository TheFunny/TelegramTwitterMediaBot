//! Inline query handling with a keystroke debounce: only a query stable for
//! [`INLINE_DEBOUNCE`] triggers a fetch, and repeats are served by Telegram's
//! inline cache instead of re-fetching.

use super::log_key;
use std::collections::HashMap;
use std::sync::LazyLock;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{
    InlineQuery, InlineQueryResult, InlineQueryResultMpeg4Gif, InlineQueryResultPhoto,
    InlineQueryResultVideo, ParseMode,
};
use x_media::media::Media;

/// Debounce window for inline queries: Telegram fires an inline query on
/// every keystroke, and each prefix of a pasted URL (e.g. `.../status/12`,
/// `.../status/123`, ...) already matches the site patterns. Without a
/// debounce every keystroke triggers a fetch (3 attempts!) of a half-typed
/// post id. Only answer once the query has been stable for this long.
const INLINE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(800);

/// How long a debounce entry is worth keeping: the window Telegram caches an
/// inline answer for (`answer_inline_query` asks for `cache_time(300)`). Past
/// it a repeat is sent to the bot again and has to be answered fresh, so the
/// entry would only suppress a fetch the user is waiting for.
const INLINE_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Last seen inline query per user and whether it was already answered.
/// Guards the debounce timer: a repeat of an answered query is served by
/// Telegram's inline cache (see `cache_time`), not by another fetch. Keyed by
/// user id — a single shared slot would let one user's typing burst (or a
/// different user's query) cancel another user's pending answer.
struct InlineDebounceState {
    query: String,
    answered: bool,
    /// When a query last touched this entry, so the periodic sweep can drop
    /// one per user who ever used inline mode (the map had no eviction at all,
    /// unlike the rate limiter's buckets and the chat store).
    last_seen: std::time::Instant,
}

#[derive(Default)]
struct DebounceStates(HashMap<u64, InlineDebounceState>);

impl DebounceStates {
    /// Records `query` as the user's newest query. Returns false when it is a
    /// repeat whose answer already went out (Telegram's inline cache serves
    /// it; re-fetching would only hit the source site again).
    fn note(&mut self, user_id: u64, query: &str) -> bool {
        if let Some(prev) = self.0.get(&user_id)
            && prev.query == query
            && prev.answered
        {
            return false;
        }
        self.0.insert(
            user_id,
            InlineDebounceState {
                query: query.to_string(),
                answered: false,
                last_seen: std::time::Instant::now(),
            },
        );
        true
    }

    /// Drops entries no query has touched for `idle_for`. Split from the clock
    /// so the boundary is testable without ageing a monotonic instant.
    fn prune_idle_at(&mut self, now: std::time::Instant, idle_for: std::time::Duration) -> usize {
        let before = self.0.len();
        self.0
            .retain(|_, state| now.saturating_duration_since(state.last_seen) < idle_for);
        before - self.0.len()
    }

    /// Claims the answer for the user's newest query; false when a newer query
    /// superseded it or the answer was already claimed.
    fn claim(&mut self, user_id: u64, query: &str) -> bool {
        let Some(state) = self.0.get_mut(&user_id) else {
            return false;
        };
        if state.query != query || state.answered {
            return false;
        }
        state.answered = true;
        state.last_seen = std::time::Instant::now();
        true
    }

    /// Releases a claimed-but-unsent answer so a repeat can retry the fetch.
    fn release(&mut self, user_id: u64, query: &str) {
        if let Some(state) = self.0.get_mut(&user_id)
            && state.query == query
        {
            state.answered = false;
            state.last_seen = std::time::Instant::now();
        }
    }
}

/// Drops debounce entries idle for [`INLINE_STATE_TTL`]; the 300 s sweep calls
/// this next to the rate limiter's prune. Returns how many were dropped.
pub(crate) fn prune_idle_states() -> usize {
    INLINE_DEBOUNCE_STATE
        .lock()
        .prune_idle_at(std::time::Instant::now(), INLINE_STATE_TTL)
}

static INLINE_DEBOUNCE_STATE: LazyLock<parking_lot::Mutex<DebounceStates>> =
    LazyLock::new(|| parking_lot::Mutex::new(DebounceStates::default()));

pub async fn inline_query_handler(bot: Bot, query: InlineQuery) -> Result<(), RequestError> {
    if query.query.is_empty() {
        return respond(());
    }
    // Only run a fetch for something that is actually a supported post URL.
    if x_media::site::cache_key(&query.query).is_none() {
        return respond(());
    }
    // Debounce: record the query and answer only after it has been stable for
    // INLINE_DEBOUNCE (the timer below). An already-answered repeat of the
    // same query is left to Telegram's inline cache instead of re-fetching.
    let user_id = query.from.id.0;
    if !INLINE_DEBOUNCE_STATE.lock().note(user_id, &query.query) {
        return respond(());
    }
    let query_text = query.query.clone();
    tokio::spawn(async move {
        tokio::time::sleep(INLINE_DEBOUNCE).await;
        // Only the user's last query of a typing burst survives: earlier
        // timers see the query changed and give up without answering.
        if !INLINE_DEBOUNCE_STATE.lock().claim(user_id, &query_text) {
            return;
        }
        match answer_inline_query(bot, query).await {
            Ok(true) => {}
            // The fetch or the answer call failed: release so a repeat of the
            // same query may retry it. An *empty* answer is a real answer
            // (`Ok(true)`), so a link whose media Telegram cannot fetch is not
            // re-fetched on every keystroke.
            Ok(false) | Err(_) => INLINE_DEBOUNCE_STATE.lock().release(user_id, &query_text),
        }
    });
    respond(())
}

/// Fetches the post behind an inline query and answers it. The caller has
/// already applied the debounce. Returns `true` when an answer was sent.
async fn answer_inline_query(bot: Bot, query: InlineQuery) -> Result<bool, RequestError> {
    // The query is user input: `debug` keeps only its normalized key, the
    // text itself is `trace` (same split as the message handler).
    log::debug!("inline query [key={}]", log_key(&query.query));
    log::trace!("inline query: {}", query.query);
    // No retries: the debounce plus a 1s/2s backoff would outlast the inline
    // query the answer belongs to.
    match x_media::site::fetch_once(&query.query).await {
        Ok(Some(fetched)) => {
            let mut results: Vec<InlineQueryResult> = Vec::new();
            // Inline results have the same 1024-char caption limit as regular
            // messages; truncate once here for all items, then apply the same
            // long-post quoting as the send paths. `answer_inline_query` has no
            // `AppContext` (the debounce spawns it), so the parsed config comes
            // from the process-wide static, and the text is the *escaped*
            // title/content the built-in caption embeds (the raw
            // `Fetched.title`/`content` differ whenever the post contains
            // `<`/`&`).
            let caption = x_media::site::truncate_caption(&fetched.caption);
            let text = fetched
                .render_fields()
                .map(|(_, _, title, content, _)| x_media::site::compose_text(title, content))
                .unwrap_or_default();
            let caption = crate::send::quote_long_caption(
                &caption,
                &text,
                super::CONFIG.caption_quote_text_chars,
            );
            for (i, media) in fetched.media.iter().enumerate() {
                let id = format!("{i}");
                // Telegram fetches an inline result's URL itself and cannot
                // send site-specific headers, so hotlink-protected media
                // (pixiv's pximg.net) would render as a broken file there.
                // Locally produced media (ugoira MP4, bsky remux) is a local
                // path and does not parse as a URL at all — same skip.
                if x_media::site::needs_media_headers(media.url()) {
                    log::debug!("inline: skipping hotlink-protected media {id}");
                    continue;
                }
                let Some(url) = url::Url::parse(media.url()).ok() else {
                    continue;
                };
                let thumbnail = media
                    .thumbnail_url()
                    .and_then(|t| url::Url::parse(t).ok())
                    .unwrap_or_else(|| url.clone());
                let caption = caption.clone().into_owned();
                let result = match media {
                    Media::Illustration { .. } => {
                        // Inline photo results have their own (smaller) size
                        // cap; use the reduced variant when one exists.
                        let photo_url = media
                            .smaller_url()
                            .and_then(|u| url::Url::parse(u).ok())
                            .unwrap_or_else(|| url.clone());
                        InlineQueryResult::Photo(
                            InlineQueryResultPhoto::new(id, photo_url, thumbnail)
                                .caption(caption)
                                .parse_mode(ParseMode::Html),
                        )
                    }
                    Media::Video { .. } => InlineQueryResult::Video(
                        InlineQueryResultVideo::new(
                            id,
                            url,
                            "video/mp4".parse().expect("valid mime"),
                            thumbnail,
                            fetched.title.clone(),
                        )
                        .caption(caption)
                        .parse_mode(ParseMode::Html),
                    ),
                    Media::Animated { .. } => InlineQueryResult::Mpeg4Gif(
                        InlineQueryResultMpeg4Gif::new(id, url, thumbnail)
                            .caption(caption)
                            .parse_mode(ParseMode::Html),
                    ),
                };
                results.push(result);
            }
            if !results.is_empty() {
                // Explicit cache window: repeats of the same query within 5
                // minutes are served by Telegram without hitting the bot.
                bot.answer_inline_query(query.id, results)
                    .cache_time(300)
                    .await?;
                return Ok(true);
            }
            // Every item was skipped: Telegram fetches an inline result's URL
            // itself, so pixiv's hotlink-protected media (and a local ugoira /
            // bsky MP4) can never be one. Answer *empty* — the client stops
            // spinning, and the same query is not re-fetched on every
            // keystroke: an unanswered query releases the debounce below
            // (`Ok(false)`), which is what made this re-run the fetch each
            // time, and the window lets Telegram serve the repeats itself.
            log::debug!("inline: nothing Telegram can fetch for the query; answering empty");
            bot.answer_inline_query(query.id, Vec::new())
                .cache_time(300)
                .await?;
            return Ok(true);
        }
        Ok(None) => {}
        Err(e) => log::error!("inline fetch [key={}]: {e}", log_key(&query.query)),
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{DebounceStates, INLINE_STATE_TTL};

    const URL_A: &str = "https://x.com/a/status/1";
    const URL_B: &str = "https://x.com/b/status/2";

    #[test]
    fn debounce_state_is_per_user() {
        let mut states = DebounceStates::default();
        // Two users query different links: both proceed, and neither timer
        // cancels the other (a single shared slot dropped one of them).
        assert!(states.note(1, URL_A));
        assert!(states.note(2, URL_B));
        assert!(states.claim(1, URL_A), "user 1's answer was cancelled");
        assert!(states.claim(2, URL_B), "user 2's answer was cancelled");
    }

    #[test]
    fn answered_query_is_suppressed_per_user_only() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A));
        assert!(states.claim(1, URL_A));
        // A repeat of the answered query by the same user is left to
        // Telegram's inline cache.
        assert!(!states.note(1, URL_A));
        // Another user pasting the same link still gets an answer.
        assert!(states.note(2, URL_A));
        assert!(states.claim(2, URL_A));
    }

    #[test]
    fn idle_states_are_pruned_and_live_ones_kept() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A));
        let first = states.0[&1].last_seen;
        // Entry 2 is strictly newer, so one timestamp can sit exactly on the
        // window's edge for one and comfortably inside it for the other.
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(states.note(2, URL_B));

        assert_eq!(
            states.prune_idle_at(first + INLINE_STATE_TTL, INLINE_STATE_TTL),
            1
        );
        assert!(
            !states.0.contains_key(&1),
            "the entry past the window must go"
        );
        assert!(states.0.contains_key(&2), "the live entry must stay");
        // A pruned user's repeat is answered fresh instead of suppressed.
        assert!(states.note(1, URL_A));
    }

    #[test]
    fn newer_query_supersedes_and_failed_answer_is_released() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A));
        assert!(states.note(1, URL_B));
        // The stale timer for the half-typed query gives up…
        assert!(!states.claim(1, URL_A));
        // …and the newest one answers.
        assert!(states.claim(1, URL_B));
        // No results → release so a repeat may retry the fetch.
        states.release(1, URL_B);
        assert!(states.claim(1, URL_B));
    }
}
