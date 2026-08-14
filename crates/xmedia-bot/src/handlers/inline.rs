//! Inline query handling with a keystroke debounce: only a query stable for
//! [`INLINE_DEBOUNCE`] triggers a fetch, and repeats are served by Telegram's
//! inline cache instead of re-fetching.

use super::log_key;
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

/// Last seen inline query and whether it was already answered. Guards the
/// debounce timer: a repeat of an answered query is served by Telegram's
/// inline cache (see `cache_time`), not by another fetch.
struct InlineDebounceState {
    query: String,
    answered: bool,
}

static INLINE_DEBOUNCE_STATE: LazyLock<parking_lot::Mutex<Option<InlineDebounceState>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));

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
    {
        let mut state = INLINE_DEBOUNCE_STATE.lock();
        if let Some(prev) = state.as_ref()
            && prev.query == query.query
            && prev.answered
        {
            return respond(());
        }
        *state = Some(InlineDebounceState {
            query: query.query.clone(),
            answered: false,
        });
    }
    let query_text = query.query.clone();
    tokio::spawn(async move {
        tokio::time::sleep(INLINE_DEBOUNCE).await;
        // Only the last query of a typing burst survives: earlier timers see
        // the query changed and give up without answering.
        {
            let mut state = INLINE_DEBOUNCE_STATE.lock();
            let Some(state) = state.as_mut() else {
                return;
            };
            if state.query != query_text || state.answered {
                return;
            }
            // Claim the answer so a repeat of the same query cannot start a
            // second fetch; reset below when no answer was produced.
            state.answered = true;
        }
        match answer_inline_query(bot, query).await {
            Ok(true) => {}
            // No results produced (or nothing to answer): let a repeat of the
            // same query retry the fetch.
            Ok(false) | Err(_) => {
                let mut state = INLINE_DEBOUNCE_STATE.lock();
                if let Some(state) = state.as_mut()
                    && state.query == query_text
                {
                    state.answered = false;
                }
            }
        }
    });
    respond(())
}

/// Fetches the post behind an inline query and answers it. The caller has
/// already applied the debounce. Returns `true` when an answer was sent.
async fn answer_inline_query(bot: Bot, query: InlineQuery) -> Result<bool, RequestError> {
    log::debug!(
        "inline query: {} [key={}]",
        query.query,
        log_key(&query.query)
    );
    match x_media::site::fetch(&query.query).await {
        Ok(Some(fetched)) => {
            let mut results: Vec<InlineQueryResult> = Vec::new();
            // Inline results have the same 1024-char caption limit as regular
            // messages; truncate once here for all items.
            let caption = x_media::site::truncate_caption(&fetched.caption);
            for (i, media) in fetched.media.iter().enumerate() {
                let id = format!("{i}");
                let Some(url) = url::Url::parse(media.url()).ok() else {
                    continue;
                };
                let thumbnail = media
                    .thumbnail_url()
                    .and_then(|t| url::Url::parse(t).ok())
                    .unwrap_or_else(|| url.clone());
                let caption = caption.clone();
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
        }
        Ok(None) => {}
        Err(e) => log::error!("inline fetch {}: {e}", query.query),
    }
    Ok(false)
}
