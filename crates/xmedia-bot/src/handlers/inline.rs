//! Inline query handling with a keystroke debounce: only a query stable for
//! [`INLINE_DEBOUNCE`] triggers a fetch, and repeats are served by Telegram's
//! inline cache instead of re-fetching.

use super::log_key;
use crate::ctx::AppContext;
use crate::link_cache::{CachedMediaKind, CachedPost};
use std::collections::HashMap;
use std::sync::LazyLock;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{
    FileId, InlineQuery, InlineQueryResult, InlineQueryResultCachedMpeg4Gif,
    InlineQueryResultCachedPhoto, InlineQueryResultCachedVideo, InlineQueryResultMpeg4Gif,
    InlineQueryResultPhoto, InlineQueryResultVideo, ParseMode,
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
    generation: u64,
    answered: bool,
    last_seen: std::time::Instant,
}

#[derive(Default)]
struct DebounceStates {
    entries: HashMap<u64, InlineDebounceState>,
    generation: u64,
}

impl DebounceStates {
    /// Records `query` as the user's newest query. Returns false when it is a
    /// repeat whose answer already went out (Telegram's inline cache serves
    /// it; re-fetching would only hit the source site again).
    fn note(&mut self, user_id: u64, query: &str) -> (bool, u64) {
        if let Some(prev) = self.entries.get(&user_id)
            && prev.query == query
            && prev.answered
        {
            return (false, prev.generation);
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.entries.insert(
            user_id,
            InlineDebounceState {
                query: query.to_string(),
                generation,
                answered: false,
                last_seen: std::time::Instant::now(),
            },
        );
        (true, generation)
    }

    /// Drops entries no query has touched for `idle_for`. Split from the clock
    /// so the boundary is testable without ageing a monotonic instant.
    fn prune_idle_at(&mut self, now: std::time::Instant, idle_for: std::time::Duration) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, state| now.saturating_duration_since(state.last_seen) < idle_for);
        before - self.entries.len()
    }
    fn claim(&mut self, user_id: u64, query: &str, generation: u64) -> bool {
        let Some(state) = self.entries.get_mut(&user_id) else {
            return false;
        };
        if state.query != query || state.generation != generation || state.answered {
            return false;
        }
        state.answered = true;
        state.last_seen = std::time::Instant::now();
        true
    }

    fn release(&mut self, user_id: u64, query: &str, generation: u64) {
        if let Some(state) = self.entries.get_mut(&user_id)
            && state.query == query
            && state.generation == generation
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
    let ctx = AppContext::from_statics(&bot);
    if query.query.is_empty() || x_media::site::cache_key(&query.query).is_none() {
        return answer_inline_query(&ctx, query).await.map(|_| ());
    }
    let user_id = query.from.id.0;
    let (should_answer, generation) = INLINE_DEBOUNCE_STATE.lock().note(user_id, &query.query);
    if !should_answer {
        return respond(());
    }
    let query_text = query.query.clone();
    tokio::spawn(async move {
        tokio::time::sleep(INLINE_DEBOUNCE).await;
        if !INLINE_DEBOUNCE_STATE
            .lock()
            .claim(user_id, &query_text, generation)
        {
            return;
        }
        let ctx = AppContext::from_statics(&bot);
        match answer_inline_query(&ctx, query).await {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                INLINE_DEBOUNCE_STATE
                    .lock()
                    .release(user_id, &query_text, generation);
            }
        }
    });
    respond(())
}

/// Answers the inline query behind a post URL. The caller has already applied
/// the debounce. Returns `true` when an answer was sent.
async fn answer_inline_query(
    ctx: &AppContext<'_>,
    query: InlineQuery,
) -> Result<bool, RequestError> {
    // The query is user input: `debug` keeps only its normalized key, the
    // text itself is `trace` (same split as the message handler).
    log::debug!("inline query [key={}]", log_key(&query.query));
    log::trace!("inline query: {}", query.query);
    let Some(key) = x_media::site::cache_key(&query.query) else {
        answer(ctx.sender, query.id, Vec::new()).await?;
        return Ok(true);
    };
    // A post that was already sent to some chat is answered from the link
    // cache: its Telegram file ids make the answer instant, and — unlike a URL
    // result, which Telegram must fetch itself — they carry media that a
    // hotlink-protected host (pixiv's pximg.net) or a locally encoded file
    // (ugoira MP4, bsky remux) can never serve inline. That media used to be
    // skipped outright, so a pixiv link answered empty.
    if let Some(cached) = ctx.link_cache.get(&key, ctx.config.link_cache_ttl).await {
        let caption = inline_caption(&cached, ctx.config.caption_quote_text_chars);
        let results = cached_inline_results(&cached, &caption);
        answer(ctx.sender, query.id, results).await?;
        return Ok(true);
    }
    // No retries: the debounce plus a 1s/2s backoff would outlast the inline
    // query the answer belongs to.
    match x_media::site::fetch_once(&query.query).await {
        Ok(Some(fetched)) => {
            // Inline results have the same 1024-char caption limit as regular
            // messages; truncate once here for all items, then apply the same
            // long-post quoting as the send paths. The built-in caption is what
            // an inline answer can use: there is no chat whose per-site format
            // could apply, so the render fields come from the fetch itself.
            let caption = x_media::site::truncate_caption(&fetched.caption);
            let text = fetched
                .render_fields()
                .map(|(_, _, title, content, _)| x_media::site::compose_text(title, content))
                .unwrap_or_default();
            let caption = crate::send::quote_long_caption(
                &caption,
                &text,
                ctx.config.caption_quote_text_chars,
            );
            let mut results: Vec<InlineQueryResult> = Vec::new();
            for (i, media) in fetched.media.iter().enumerate().take(50) {
                // Telegram fetches an inline result's URL itself and cannot
                // send site-specific headers, so hotlink-protected media
                // (pixiv's pximg.net) would render as a broken file there.
                // Locally produced media (ugoira MP4, bsky remux) is a local
                // path and does not parse as a URL at all — same skip.
                if x_media::site::needs_media_headers(media.url()) {
                    log::debug!("inline: skipping hotlink-protected media {i}");
                    continue;
                }
                let Some(url) = url::Url::parse(media.url()).ok() else {
                    continue;
                };
                let thumbnail = media
                    .thumbnail_url()
                    .and_then(|t| url::Url::parse(t).ok())
                    .unwrap_or_else(|| url.clone());
                // Inline photo results have their own (smaller) size cap; use
                // the reduced variant when one exists.
                let url = media
                    .smaller_url()
                    .and_then(|u| url::Url::parse(u).ok())
                    .unwrap_or(url);
                results.push(url_result(
                    i.to_string(),
                    match media {
                        Media::Illustration { .. } => CachedMediaKind::Photo,
                        Media::Video { .. } => CachedMediaKind::Video,
                        Media::Animated { .. } => CachedMediaKind::Animation,
                    },
                    url,
                    thumbnail,
                    fetched.title.clone(),
                    caption.clone().into_owned(),
                ));
            }
            // Every item was skipped, or the post has no media at all: answer
            // *empty* rather than leaving the query unanswered (a client keeps
            // spinning on that, and the debounce's release re-runs the fetch on
            // every keystroke).
            answer(ctx.sender, query.id, results).await?;
            return Ok(true);
        }
        Ok(None) | Err(_) => {
            if let Err(e) = answer(ctx.sender, query.id, Vec::new()).await {
                log::error!(
                    "inline empty answer failed for [key={}]: {e}",
                    log_key(&query.query)
                );
                return Err(e);
            }
            return Ok(true);
        }
    }
}

/// The caption of an inline answer, from a cached post: the caption that was
/// sent (the site's built-in one, truncated) plus the long-post quoting the
/// send paths apply.
fn inline_caption(cached: &CachedPost, quote_chars: usize) -> String {
    let text = x_media::site::compose_text(&cached.title, &cached.content);
    crate::send::quote_long_caption(
        &x_media::site::truncate_caption(&cached.caption),
        &text,
        quote_chars,
    )
    .into_owned()
}

/// One inline result pointing Telegram at a URL it fetches itself.
fn url_result(
    id: String,
    kind: CachedMediaKind,
    url: url::Url,
    thumbnail: url::Url,
    title: String,
    caption: String,
) -> InlineQueryResult {
    let parse_mode = ParseMode::Html;
    match kind {
        CachedMediaKind::Photo => InlineQueryResult::Photo(
            InlineQueryResultPhoto::new(id, url, thumbnail)
                .caption(caption)
                .parse_mode(parse_mode),
        ),
        CachedMediaKind::Video => InlineQueryResult::Video(
            InlineQueryResultVideo::new(
                id,
                url,
                "video/mp4".parse().expect("valid mime"),
                thumbnail,
                title,
            )
            .caption(caption)
            .parse_mode(parse_mode),
        ),
        CachedMediaKind::Animation => InlineQueryResult::Mpeg4Gif(
            InlineQueryResultMpeg4Gif::new(id, url, thumbnail)
                .caption(caption)
                .parse_mode(parse_mode),
        ),
    }
}

/// One inline result served from a Telegram file id.
fn cached_result(
    id: String,
    kind: CachedMediaKind,
    file_id: String,
    title: String,
    caption: String,
) -> InlineQueryResult {
    let parse_mode = ParseMode::Html;
    let file_id = FileId(file_id);
    match kind {
        CachedMediaKind::Photo => InlineQueryResult::CachedPhoto(
            InlineQueryResultCachedPhoto::new(id, file_id)
                .caption(caption)
                .parse_mode(parse_mode),
        ),
        CachedMediaKind::Video => InlineQueryResult::CachedVideo(
            InlineQueryResultCachedVideo::new(id, file_id, title)
                .caption(caption)
                .parse_mode(parse_mode),
        ),
        CachedMediaKind::Animation => InlineQueryResult::CachedMpeg4Gif(
            InlineQueryResultCachedMpeg4Gif::new(id, file_id)
                .caption(caption)
                .parse_mode(parse_mode),
        ),
    }
}

/// The inline results a cached post answers with, one per media item: from the
/// file id when the entry has one, else from the source URL (a degraded entry
/// keeps only URLs). A URL item that needs site headers is skipped as in the
/// fetch path; a *file id* needs no headers, which is what makes a pixiv post
/// answerable inline.
fn cached_inline_results(cached: &CachedPost, caption: &str) -> Vec<InlineQueryResult> {
    cached
        .media
        .iter()
        .enumerate()
        .take(50)
        .filter_map(|(i, media)| {
            let id = i.to_string();
            let caption = || caption.to_string();
            if !media.file_id.is_empty() {
                return Some(cached_result(
                    id,
                    media.kind,
                    media.file_id.clone(),
                    cached.title.clone(),
                    caption(),
                ));
            }
            // A degraded entry: no file id, so Telegram must fetch the URL.
            if x_media::site::needs_media_headers(&media.url) {
                log::debug!("inline: skipping hotlink-protected cached media {i}");
                return None;
            }
            let url = url::Url::parse(&media.url).ok()?;
            // A degraded entry has no poster, and Telegram would try to render
            // a video URL as its own thumbnail — skip it. A photo or gif is an
            // image, so its own URL serves as the thumbnail.
            if matches!(media.kind, CachedMediaKind::Video) {
                log::debug!("inline: skipping a cached video with no thumbnail {i}");
                return None;
            }
            Some(url_result(
                id,
                media.kind,
                url.clone(),
                url,
                cached.title.clone(),
                caption(),
            ))
        })
        .collect()
}

/// Answers with `results` (an empty vec is a real answer: it stops the client
/// spinning and lets Telegram serve repeats itself) under the cache window
/// [`INLINE_STATE_TTL`] mirrors.
async fn answer(
    sender: &dyn crate::media_sender::MediaSender,
    id: teloxide::types::InlineQueryId,
    results: Vec<InlineQueryResult>,
) -> Result<(), RequestError> {
    if results.is_empty() {
        log::debug!("inline: nothing Telegram can serve for the query; answering empty");
    }
    sender.answer_inline_query(id, results, 300).await
}

#[cfg(test)]
mod tests {
    use super::{DebounceStates, INLINE_STATE_TTL, answer_inline_query, cached_inline_results};
    use crate::ctx::test_support::{TestStores, api_error, cached_photo};
    use crate::link_cache::{CachedMedia, CachedMediaKind};
    use crate::media_sender::test_support::MockSender;
    use teloxide::types::InlineQuery;

    const URL_A: &str = "https://x.com/a/status/1";
    const URL_B: &str = "https://x.com/b/status/2";

    fn inline_query(url: &str) -> InlineQuery {
        serde_json::from_value(serde_json::json!({
            "id": "42",
            "from": { "id": 5, "is_bot": false, "first_name": "u" },
            "query": url,
            "offset": "",
        }))
        .expect("a minimal inline query deserializes")
    }

    /// A post already in the link cache is answered from its file ids: no
    /// fetch, and — unlike a URL result — media Telegram could never fetch
    /// itself (a pixiv pximg URL) can be served.
    #[tokio::test]
    async fn a_cached_post_answers_from_its_file_ids() {
        let sender = MockSender::scripted(vec![], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let mut entry = cached_photo();
        entry.media = vec![
            CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: "AgAC-photo".into(),
                url: "https://i.pximg.net/img-original/img/1.jpg".into(),
            },
            CachedMedia {
                kind: CachedMediaKind::Animation,
                file_id: "AgAC-gif".into(),
                url: "https://i.pximg.net/img-original/img/1.gif".into(),
            },
        ];
        stores.link_cache().put("twitter:1", &entry).await;

        let answered = answer_inline_query(&ctx, inline_query("https://x.com/u/status/1"))
            .await
            .unwrap();

        assert!(answered);
        assert_eq!(
            sender.inline_answers(),
            vec![vec!["cached_photo:AgAC-photo", "cached_gif:AgAC-gif"]],
            "every item goes out as its cached file id, hotlink protection and all"
        );
    }

    /// A degraded entry has no file ids left, so its URLs are used — and an
    /// item Telegram must not fetch (needs site headers) or cannot render (a
    /// video with no poster) is skipped. Nothing left means an *empty* answer:
    /// leaving the query unanswered makes the client spin and re-fetch on every
    /// keystroke.
    #[tokio::test]
    async fn a_degraded_cached_post_answers_with_urls_or_empty() {
        let sender = MockSender::scripted(vec![], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);

        let mut entry = cached_photo();
        entry.media = vec![
            CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: String::new(),
                url: "https://p/1.jpg".into(),
            },
            CachedMedia {
                kind: CachedMediaKind::Video,
                file_id: String::new(),
                url: "https://v/1.mp4".into(),
            },
        ];
        stores.link_cache().put("twitter:1", &entry).await;
        answer_inline_query(&ctx, inline_query("https://x.com/u/status/1"))
            .await
            .unwrap();
        assert_eq!(
            sender.inline_answers(),
            vec![vec!["photo:https://p/1.jpg"]],
            "the degradable photo goes out by URL, the poster-less video is skipped"
        );

        // Nothing servable: a pixiv original needs a Referer Telegram does not
        // send.
        stores.link_cache().remove("twitter:1").await;
        let mut entry = cached_photo();
        entry.media = vec![CachedMedia {
            kind: CachedMediaKind::Photo,
            file_id: String::new(),
            url: "https://i.pximg.net/img-original/img/1.jpg".into(),
        }];
        stores.link_cache().put("twitter:1", &entry).await;
        answer_inline_query(&ctx, inline_query("https://x.com/u/status/1"))
            .await
            .unwrap();
        assert_eq!(
            sender.inline_answers(),
            vec![vec!["photo:https://p/1.jpg".to_string()], Vec::new()],
            "a query with nothing servable is still answered, with no results"
        );
    }

    #[test]
    fn inline_results_are_capped_at_telegram_limit() {
        let mut entry = cached_photo();
        entry.media = (0..51)
            .map(|i| CachedMedia {
                kind: CachedMediaKind::Photo,
                file_id: format!("id-{i}"),
                url: format!("https://p/{i}.jpg"),
            })
            .collect();
        assert_eq!(cached_inline_results(&entry, "caption").len(), 50);
    }

    #[test]
    fn debounce_state_is_per_user() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A).0);
        assert!(states.note(2, URL_B).0);
        let (_, generation_a) = states.note(1, URL_A);
        let (_, generation_b) = states.note(2, URL_B);
        assert!(states.claim(1, URL_A, generation_a));
        assert!(states.claim(2, URL_B, generation_b));
    }

    #[test]
    fn answered_query_is_suppressed_per_user_only() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A).0);
        let (_, generation) = states.note(1, URL_A);
        assert!(states.claim(1, URL_A, generation));
        assert!(!states.note(1, URL_A).0);
        assert!(states.note(2, URL_A).0);
        let (_, generation) = states.note(2, URL_A);
        assert!(states.claim(2, URL_A, generation));
    }

    #[test]
    fn idle_states_are_pruned_and_live_ones_kept() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A).0);
        let first = states.entries[&1].last_seen;
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(states.note(2, URL_B).0);

        assert_eq!(
            states.prune_idle_at(first + INLINE_STATE_TTL, INLINE_STATE_TTL),
            1
        );
        assert!(!states.entries.contains_key(&1));
        assert!(states.entries.contains_key(&2));
        assert!(states.note(1, URL_A).0);
    }

    #[test]
    fn newer_query_supersedes_and_failed_answer_is_released() {
        let mut states = DebounceStates::default();
        assert!(states.note(1, URL_A).0);
        let (_, generation_a) = states.note(1, URL_B);
        assert!(!states.claim(1, URL_A, generation_a));
        let (_, generation_b) = states.note(1, URL_B);
        assert!(states.claim(1, URL_B, generation_b));
        states.release(1, URL_B, generation_b);
        assert!(states.claim(1, URL_B, generation_b));
    }
}
