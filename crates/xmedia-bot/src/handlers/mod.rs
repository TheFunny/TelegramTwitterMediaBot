//! Update handlers and the per-URL media pipeline.
//!
//! Split into per-concern modules: [`commands`] (the `/`-command executor),
//! [`urls`] (URL extraction + the bounded worker pool + send dispatch),
//! [`inline`] (debounced inline queries), [`callback`] (edit-before-forward
//! buttons) and [`statics`] (the shared process-wide stores). This module
//! holds the message entry point and the helpers the others share.

mod callback;
mod commands;
mod inline;
mod repair;
mod statics;
mod url_workers;
mod urls;

pub use callback::callback_query_handler;
pub use commands::register_commands;
pub use inline::inline_query_handler;
pub(crate) use inline::prune_idle_states;
pub(crate) use repair::repair_lost_local_media;
/// The resolved `$DATA_DIR/task_queue.db` path, for the startup config line.
pub(crate) use statics::db_path;
pub use statics::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE};
pub use url_workers::{start_url_workers, stop_url_workers};

use crate::ctx::AppContext;
use crate::media_sender::MediaSender;
use commands::{Command, execute_command};
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Message, MessageId};
use teloxide::utils::command::BotCommands;
use url_workers::URL_JOBS;
use urls::extract_urls;

/// Reply to a message by id, keeping the reply decoration even if the
/// original was already deleted.
pub(crate) async fn reply(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: MessageId,
    text: impl Into<String>,
) -> Result<(), RequestError> {
    sender
        .send_message(ChatId(chat_id), text.into(), Some(reply_to), None)
        .await
        .map(|_| ())
}

/// Log prefix tying the whole lifecycle of one link (fetch → send → cache →
/// forward) together: the normalized cache key (`twitter:123…`, `pixiv:123`,
/// `bsky:handle/rkey`, `bilibili:123…`) instead of the raw URL, so logs stay
/// short and do not echo full user-submitted URLs at info level.
pub fn log_key(url: &str) -> String {
    x_media::site::cache_key(url).unwrap_or_else(|| "<unsupported>".to_string())
}

/// Makes user-supplied text (a display name, callback data, a channel handle)
/// fit one log line: newlines and other control characters are escaped, so a
/// crafted value cannot forge a second log entry or hide inside one. Tab is
/// kept — it cannot break the line.
pub(crate) fn log_escape(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c != '\t' && c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// How long a caption edit may sleep before it gives up on retrying: the reply
/// (or button press) that carried the text is already consumed, so the update
/// must not stall the chat's queue behind a long flood-control wait — the user
/// is told to send it again instead.
const CAPTION_EDIT_MAX_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a caption edit landed.
enum EditOutcome {
    Applied,
    /// The API's reason, for the message the user gets.
    Failed(String),
}

/// Applies a caption edit, retrying once when the API names a short retryable
/// delay (`RetryAfter`/network/5xx). A failed edit used to be logged and
/// swallowed while the record was updated anyway: the user saw nothing, the
/// caption never changed, and the text they typed was gone. Callers report
/// [`EditOutcome::Failed`] instead.
async fn apply_caption_edit(
    sender: &dyn MediaSender,
    chat_id: ChatId,
    message_id: MessageId,
    caption: String,
) -> EditOutcome {
    let mut attempt = 0;
    loop {
        match sender
            .edit_message_caption(chat_id, message_id, caption.clone())
            .await
        {
            Ok(()) => return EditOutcome::Applied,
            Err(e) => {
                let reason = e.to_string();
                if attempt == 0
                    && let crate::send::Classification::Retryable { delay_seconds } =
                        crate::send::classify_request_error(&e)
                    && std::time::Duration::from_secs_f64(delay_seconds)
                        <= CAPTION_EDIT_MAX_RETRY_WAIT
                {
                    attempt = 1;
                    log::debug!("caption edit failed ({reason}), retrying once");
                    tokio::time::sleep(std::time::Duration::from_secs_f64(delay_seconds)).await;
                    continue;
                }
                log::error!("edit_message_caption failed: {reason}");
                return EditOutcome::Failed(reason);
            }
        }
    }
}

/// Edit-before-forward: a reply to the prompt swaps the caption of the first
/// forwarded message. Returns true when the message was consumed as an edit.
/// Body of [`message_handler`]'s edit branch, without teloxide update types so
/// it can be driven by tests.
async fn edit_message_handler(
    ctx: &AppContext<'_>,
    chat_id: i64,
    reply_to_message_id: i64,
    text: &str,
) -> bool {
    let chat_data = ctx.chat_store.get(chat_id).await;
    let Some(edit) = chat_data.edit_message.get(&reply_to_message_id) else {
        return false;
    };
    // Lazy expiry, the same rule a button press gets: a record past the TTL
    // (not yet swept) is dropped and the reply falls through to the normal
    // message flow instead of rewriting a caption from a dead prompt.
    if edit.created_at + ctx.config.edit_message_ttl.as_secs() as i64 <= crate::db::unix_now() {
        ctx.chat_store
            .update(chat_id, |data| {
                data.edit_message.remove(&reply_to_message_id);
            })
            .await;
        return false;
    }
    let Some(first_forward_id) = edit.forward_message_ids.first() else {
        return false;
    };
    let link = format!(
        "<a href=\"{0}\">{1}</a>",
        html_escape::encode_double_quoted_attribute(&edit.url),
        html_escape::encode_text(text)
    );
    let new_text = if edit.template.is_empty() {
        link
    } else {
        chat_data
            .template
            .get(&edit.template)
            .map(|template| template.replace("[]", &link))
            .unwrap_or(link)
    };
    match apply_caption_edit(
        ctx.sender,
        ChatId(chat_id),
        MessageId(*first_forward_id as i32),
        new_text,
    )
    .await
    {
        EditOutcome::Applied => log::info!(
            "edit-before-forward: caption swapped on message {first_forward_id} for prompt {reply_to_message_id}"
        ),
        // The reply was a caption for this prompt, so it stays consumed either
        // way — but the user is told the swap failed instead of losing it
        // silently (and can send it again).
        EditOutcome::Failed(reason) => {
            let _ = reply(
                ctx.sender,
                chat_id,
                MessageId(reply_to_message_id as i32),
                format!("Could not update the caption ({reason}). Send it again to retry."),
            )
            .await;
        }
    }
    true
}

/// The `dptree` entry point: the process-wide context, plus the bot the
/// dispatcher handed us (used for the replies this module sends itself).
pub async fn message_handler(bot: Bot, message: Message) -> Result<(), RequestError> {
    handle_message(&AppContext::from_statics(&bot), &bot, message).await
}

/// Body of [`message_handler`], taking its context. Every branch here — the
/// edit-reply interception, the command path, the private-chat link enqueue and
/// the group hint — is otherwise reachable only through the process-wide
/// statics, which is why none of them had a test.
pub(crate) async fn handle_message(
    ctx: &AppContext<'_>,
    bot: &Bot,
    message: Message,
) -> Result<(), RequestError> {
    let is_private = message.chat.is_private();
    let sender = message
        .from
        .as_ref()
        .map(|from| from.full_name())
        .unwrap_or_else(|| "unknown".to_string());
    let text_preview = message
        .text()
        .map(|t| {
            let end = t.floor_char_boundary(120.min(t.len()));
            &t[..end]
        })
        .unwrap_or("<no text>");
    // Per-request detail: who and where at `debug`; the message text itself is
    // user data and only ever appears at `trace`, so a `debug` log can be
    // shared without leaking what people pasted.
    log::debug!(
        "message from {sender} in {} (private={is_private})",
        message.chat.id
    );
    log::trace!("message text: {text_preview}");
    // URL/edit flows only run in private chats; commands run in any chat.
    if is_private
        && let Some(reply) = message.reply_to_message()
        && let Some(text) = message.text()
        && edit_message_handler(ctx, message.chat.id.0, reply.id.0 as i64, text).await
    {
        return respond(());
    }
    if let Some(text) = message.text()
        && let Ok(command) = Command::parse(text, "")
    {
        // The command name is what the operator needs at `debug`; its argument
        // may be a user-supplied URL, which stays at `trace`.
        log::debug!(
            "command from {}: {}",
            message.chat.id,
            text.split_whitespace().next().unwrap_or("<empty>")
        );
        log::trace!("command text: {text_preview}");
        execute_command(ctx, bot, &message, command).await?;
        return respond(());
    }
    if is_private {
        // Only links a site adapter claims: an unsupported URL never gets a
        // media message, so enqueuing it would spend a queue slot, a worker
        // wake-up and (through `run_with_chat_action`) a Telegram call on
        // nothing. Same test the group branch below makes for its hint.
        let urls: Vec<String> = extract_urls(&message)
            .into_iter()
            .filter(|url| x_media::site::cache_key(url).is_some())
            .collect();
        if !urls.is_empty() {
            // Debug only, and echo the normalized keys instead of the raw URLs.
            let keys: Vec<String> = urls.iter().map(|u| log_key(u)).collect();
            log::debug!("queuing {} supported URL(s): {keys:?}", urls.len());
        }
        for url in urls {
            // Clone out of the lock: the parking_lot guard is !Send and must
            // not be held across the await below.
            let Some(tx) = URL_JOBS.lock().clone() else {
                log::warn!("url workers not started; dropping link");
                break;
            };
            // A closed channel means the workers are stopping (shutdown):
            // report the dropped link instead of losing it silently.
            if tx.send((message.clone(), url)).await.is_err() {
                log::warn!("url workers stopped; dropping link");
                break;
            }
        }
    } else if (message.chat.is_group() || message.chat.is_supergroup())
        && extract_urls(&message)
            .iter()
            .any(|url| x_media::site::cache_key(url).is_some())
    {
        // A supported link in a group used to be dropped in silence, which
        // reads as a broken bot (the command menu is registered globally, so
        // the expectation is there). Unsupported links stay ignored; the hint
        // names the two paths that do work. Channels are excluded — the reply
        // would be posted into the channel itself.
        let _ = reply(ctx.sender, message.chat.id.0, message.id, GROUP_LINK_HINT).await;
    }
    respond(())
}

/// Answer for a link posted where the pipeline does not run (a group): links
/// are private-chat only, inline mode is the group path.
const GROUP_LINK_HINT: &str =
    "Links are handled in private chat only — send me this link there, or use inline mode here.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::{FORWARDED_ID, PROMPT_ID, TestStores, api_error, seed_prompt};
    use crate::media_sender::test_support::{MockSender, Outcome};
    use teloxide::RequestError;

    /// The Telegram wording the mocks answer with: a message the bot cannot
    /// edit (the prompt was deleted).
    const API_ERROR: &str = "Bad Request: message not found";

    #[test]
    fn log_escape_cannot_forge_a_second_log_line() {
        let forged = log_escape("alice\nINFO injected entry");
        assert!(!forged.contains('\n'), "no raw newline may survive");
        assert!(
            forged.contains("\\n"),
            "the break stays visible as an escape"
        );
        // The common case (clean input) borrows — logging must not allocate.
        assert!(matches!(
            log_escape("plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[tokio::test]
    async fn a_reply_to_an_expired_prompt_is_not_edited() {
        // 90 000 s ago: past the TTL under any config a test can hold.
        let sender = MockSender::scripted(vec![], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now() - 90_000).await;

        let consumed = edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await;

        assert!(!consumed, "an expired prompt must not consume the reply");
        assert!(
            sender.captions().is_empty(),
            "no caption edit may reach a dead prompt"
        );
        assert!(
            stores.chat_store().get(1).await.edit_message.is_empty(),
            "the stale record must be dropped for good"
        );
    }

    #[tokio::test]
    async fn reply_to_a_prompt_swaps_the_caption_through_its_template() {
        let sender = MockSender::scripted(vec![Outcome::EditOk], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now()).await;

        let consumed = edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await;

        assert!(consumed, "a reply to the prompt must be consumed");
        assert_eq!(
            sender.captions(),
            vec!["<b><a href=\"https://x.com/u/status/1\">new caption</a></b>"]
        );
    }

    #[tokio::test]
    async fn reply_text_and_url_are_escaped_into_the_caption() {
        let sender = MockSender::scripted(vec![Outcome::EditOk], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        edit_message_handler(&ctx, 1, PROMPT_ID, "<script>alert(1)</script>").await;

        // No raw markup from user text may reach the HTML caption.
        assert_eq!(
            sender.captions(),
            vec!["<a href=\"https://x.com/u/status/1\">&lt;script&gt;alert(1)&lt;/script&gt;</a>"]
        );
    }

    #[tokio::test]
    async fn a_failed_caption_swap_is_reported_and_consumed() {
        // The script is per call, in order: the edit fails, the notice follows.
        let sender = MockSender::scripted(vec![Outcome::EditErr, Outcome::MessageOk], || {
            api_error(API_ERROR)
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now()).await;

        // The edit failed (message deleted etc.); the reply must still be
        // swallowed instead of being treated as a link to fetch — and the user
        // must be told, because the text they sent is gone either way.
        assert!(edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await);
        assert_eq!(sender.calls(), vec!["edit_message_caption", "send_message"]);
        let notice = sender.messages().join(" ");
        assert!(notice.contains("Could not update the caption"), "{notice}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_retryable_caption_failure_is_retried_once() {
        use teloxide::types::Seconds;
        // A one-second flood-control wait is worth honouring: the retry lands
        // and the user never hears about it.
        let sender = MockSender::scripted(vec![Outcome::EditErr, Outcome::EditOk], || {
            RequestError::RetryAfter(Seconds::from_seconds(1))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now()).await;

        assert!(edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await);
        assert_eq!(
            sender.calls(),
            vec!["edit_message_caption", "edit_message_caption"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_retryable_caption_failure_is_not_retried() {
        use teloxide::types::Seconds;
        // A minute-long wait must not stall the chat's update queue behind it:
        // the user is told to send the caption again instead.
        let sender = MockSender::scripted(vec![Outcome::EditErr, Outcome::MessageOk], || {
            RequestError::RetryAfter(Seconds::from_seconds(60))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now()).await;

        assert!(edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await);
        assert_eq!(sender.calls(), vec!["edit_message_caption", "send_message"]);
    }

    #[tokio::test]
    async fn reply_to_an_unrelated_message_is_not_consumed() {
        let sender = MockSender::scripted(vec![], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);

        // No prompt record for that message id → the reply runs the normal
        // (URL/command) path instead.
        assert!(!edit_message_handler(&ctx, 1, PROMPT_ID, "hello").await);
        assert!(sender.calls().is_empty());
    }

    /// A reply driven through the real message entry point into a real `Bot`:
    /// the routing (reply-to-prompt → caption swap, before the command and URL
    /// branches) and the request teloxide builds.
    #[tokio::test]
    async fn a_prompt_reply_reaches_the_api_as_a_caption_edit() {
        use crate::media_sender::test_support::fake_api::FakeApi;
        use teloxide::Bot;

        let api = FakeApi::start().await;
        let bot = Bot::new("42:TEST").set_api_url(api.url());
        let stores = TestStores::new();
        let ctx = stores.ctx(&bot);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;
        let message: Message = serde_json::from_value(serde_json::json!({
            "message_id": PROMPT_ID + 1,
            "date": 0,
            "chat": { "id": 1, "type": "private" },
            "from": { "id": 5, "is_bot": false, "first_name": "u" },
            "reply_to_message": {
                "message_id": PROMPT_ID,
                "date": 0,
                "chat": { "id": 1, "type": "private" },
                "text": "prompt",
            },
            "text": "new caption",
        }))
        .expect("a minimal message deserializes");

        handle_message(&ctx, &bot, message).await.unwrap();

        assert_eq!(api.methods(), vec!["EditMessageCaption"]);
        let body = api.body("EditMessageCaption");
        assert_eq!(body["chat_id"], 1);
        assert_eq!(body["message_id"], FORWARDED_ID);
        assert_eq!(
            body["caption"],
            "<a href=\"https://x.com/u/status/1\">new caption</a>"
        );

        // The other branch of the same entry point: a supported link in a group
        // gets the one explanatory reply (the link pipeline is private-chat only,
        // and dropping it in silence reads as a broken bot).
        let group: Message = serde_json::from_value(serde_json::json!({
            "message_id": 2,
            "date": 0,
            "chat": { "id": -100, "type": "group", "title": "g" },
            "from": { "id": 5, "is_bot": false, "first_name": "u" },
            "text": "https://x.com/u/status/1",
            "entities": [{ "type": "url", "offset": 0, "length": 24 }],
        }))
        .expect("a minimal group message deserializes");

        handle_message(&ctx, &bot, group).await.unwrap();

        assert_eq!(api.methods(), vec!["EditMessageCaption", "SendMessage"]);
        assert_eq!(api.body("SendMessage")["text"], GROUP_LINK_HINT);

        // A channel stays silent: the hint reply would be posted into the
        // channel itself, so the same link must produce no further call.
        let channel: Message = serde_json::from_value(serde_json::json!({
            "message_id": 3,
            "date": 0,
            "chat": { "id": -1001234567890i64, "type": "channel", "title": "c" },
            "text": "https://x.com/u/status/1",
            "entities": [{ "type": "url", "offset": 0, "length": 24 }],
        }))
        .expect("a minimal channel message deserializes");

        handle_message(&ctx, &bot, channel).await.unwrap();

        assert_eq!(
            api.methods(),
            vec!["EditMessageCaption", "SendMessage"],
            "a channel must not get the group hint"
        );

        // And an *unsupported* link in a group stays silent too: the hint is
        // for links a site adapter claims (the branch's own filter).
        let unsupported: Message = serde_json::from_value(serde_json::json!({
            "message_id": 4,
            "date": 0,
            "chat": { "id": -100, "type": "group", "title": "g" },
            "text": "https://example.com/x",
            "entities": [{ "type": "url", "offset": 0, "length": 19 }],
        }))
        .expect("a minimal group message deserializes");

        handle_message(&ctx, &bot, unsupported).await.unwrap();

        assert_eq!(
            api.methods(),
            vec!["EditMessageCaption", "SendMessage"],
            "an unsupported link must not get the hint"
        );
    }
}
