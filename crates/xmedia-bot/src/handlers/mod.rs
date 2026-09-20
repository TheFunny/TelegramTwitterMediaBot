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
mod statics;
mod urls;

pub use callback::callback_query_handler;
pub use commands::register_commands;
pub use inline::inline_query_handler;
pub(crate) use inline::prune_idle_states;
/// The resolved `$DATA_DIR/task_queue.db` path, for the startup config line.
pub(crate) use statics::db_path;
pub use statics::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE};
pub use urls::{start_url_workers, stop_url_workers};

use crate::ctx::AppContext;
use crate::media_sender::MediaSender;
use commands::{Command, execute_command};
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, ChatKind, Message, MessageId, ParseMode, PublicChatKind, ReplyParameters,
};
use teloxide::utils::command::BotCommands;
use urls::{URL_JOBS, extract_urls};

/// Reply to a message by id, keeping the reply decoration even if the
/// original was already deleted. Returns the reply's message id.
pub(crate) async fn reply(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: MessageId,
    text: impl Into<String>,
) -> Result<i64, RequestError> {
    sender
        .send_message(ChatId(chat_id), text.into(), Some(reply_to), None)
        .await
}

/// Reply to a message by id with HTML parse mode (same reply decoration as
/// [`reply`]). Used by `/test`, whose report is an HTML message (the caption
/// is wrapped in a `<blockquote>` to show it exactly as it will render).
pub(crate) async fn reply_html(
    bot: &Bot,
    chat_id: i64,
    reply_to: MessageId,
    text: String,
) -> Result<i64, RequestError> {
    // `<Bot as Requester>::` disambiguates from the MediaSender trait's
    // same-named method (see media_sender.rs).
    <Bot as Requester>::send_message(bot, ChatId(chat_id), text)
        .parse_mode(ParseMode::Html)
        .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply())
        .await
        .map(|message| message.id.0 as i64)
}

/// Log prefix tying the whole lifecycle of one link (fetch → send → cache →
/// forward) together: the normalized cache key (`twitter:123…`, `pixiv:123`,
/// `bsky:handle/rkey`, `bilibili:123…`) instead of the raw URL, so logs stay
/// short and do not echo full user-submitted URLs at info level.
pub fn log_key(url: &str) -> String {
    x_media::site::cache_key(url).unwrap_or_else(|| "<unsupported>".to_string())
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
    match ctx
        .sender
        .edit_message_caption(
            ChatId(chat_id),
            MessageId(*first_forward_id as i32),
            new_text,
        )
        .await
    {
        Ok(()) => log::info!(
            "edit-before-forward: caption swapped on message {first_forward_id} for prompt {reply_to_message_id}"
        ),
        Err(e) => log::error!("edit_message_caption failed: {e}"),
    }
    true
}

pub async fn message_handler(bot: Bot, message: Message) -> Result<(), RequestError> {
    let is_private = matches!(message.chat.kind, ChatKind::Private(_));
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
        && edit_message_handler(
            &AppContext::from_statics(&bot),
            message.chat.id.0,
            reply.id.0 as i64,
            text,
        )
        .await
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
        execute_command(&bot, &message, command).await?;
        return respond(());
    }
    if is_private {
        let urls = extract_urls(&message);
        if !urls.is_empty() {
            // Debug only, and echo the normalized keys instead of the raw URLs.
            let keys: Vec<String> = urls.iter().map(|u| log_key(u)).collect();
            log::debug!("extracted {} URL(s): {keys:?}", urls.len());
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
    } else if is_group(&message.chat.kind)
        && extract_urls(&message)
            .iter()
            .any(|url| x_media::site::cache_key(url).is_some())
    {
        // A supported link in a group used to be dropped in silence, which
        // reads as a broken bot (the command menu is registered globally, so
        // the expectation is there). Unsupported links stay ignored; the hint
        // names the two paths that do work. Channels are excluded — the reply
        // would be posted into the channel itself.
        let _ = reply(&bot, message.chat.id.0, message.id, GROUP_LINK_HINT).await;
    }
    respond(())
}

/// Answer for a link posted where the pipeline does not run (a group): links
/// are private-chat only, inline mode is the group path.
const GROUP_LINK_HINT: &str =
    "Links are handled in private chat only — send me this link there, or use inline mode here.";

/// Groups and supergroups, as opposed to private chats and channels.
fn is_group(kind: &ChatKind) -> bool {
    matches!(
        kind,
        ChatKind::Public(chat)
            if matches!(
                chat.kind,
                PublicChatKind::Group | PublicChatKind::Supergroup(_)
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::{PROMPT_ID, TestStores, api_error, seed_prompt};
    use crate::media_sender::test_support::{MockSender, Outcome};

    /// The Telegram wording the mocks answer with: a message the bot cannot
    /// edit (the prompt was deleted).
    const API_ERROR: &str = "Bad Request: message not found";

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
    async fn a_failed_caption_swap_still_consumes_the_reply() {
        let sender = MockSender::scripted(vec![Outcome::EditErr], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "tpl", crate::db::unix_now()).await;

        // The edit failed (message deleted etc.); the reply must still be
        // swallowed instead of being treated as a link to fetch.
        assert!(edit_message_handler(&ctx, 1, PROMPT_ID, "new caption").await);
        assert_eq!(sender.calls(), vec!["edit_message_caption"]);
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

    #[test]
    fn the_link_hint_is_for_groups_only() {
        use teloxide::types::{ChatPrivate, ChatPublic, PublicChatChannel, PublicChatSupergroup};

        let group = ChatKind::Public(ChatPublic {
            title: None,
            kind: PublicChatKind::Group,
        });
        let supergroup = ChatKind::Public(ChatPublic {
            title: None,
            kind: PublicChatKind::Supergroup(PublicChatSupergroup {
                username: None,
                is_forum: false,
            }),
        });
        // A channel must stay silent: the hint reply would be posted into the
        // channel itself.
        let channel = ChatKind::Public(ChatPublic {
            title: None,
            kind: PublicChatKind::Channel(PublicChatChannel { username: None }),
        });
        let private = ChatKind::Private(ChatPrivate {
            username: None,
            first_name: None,
            last_name: None,
        });

        assert!(is_group(&group));
        assert!(is_group(&supergroup));
        assert!(!is_group(&channel));
        assert!(!is_group(&private));
    }
}
