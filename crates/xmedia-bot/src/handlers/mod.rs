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
pub use statics::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE};
pub use urls::{start_url_workers, stop_url_workers};

use crate::media_sender::MediaSender;
use commands::{Command, execute_command};
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatKind, Message, MessageId, ParseMode, ReplyParameters};
use teloxide::utils::command::BotCommands;
use urls::{URL_JOBS, extract_urls};

/// Reply to a message by id, keeping the reply decoration even if the
/// original was already deleted.
pub(crate) async fn reply<T>(
    sender: &dyn MediaSender,
    chat_id: i64,
    reply_to: MessageId,
    text: T,
) -> Result<Message, RequestError>
where
    T: Into<String>,
{
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
) -> Result<Message, RequestError> {
    // `<Bot as Requester>::` disambiguates from the MediaSender trait's
    // same-named method (see media_sender.rs).
    <Bot as Requester>::send_message(bot, ChatId(chat_id), text)
        .parse_mode(ParseMode::Html)
        .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply())
        .await
}

/// Log prefix tying the whole lifecycle of one link (fetch → send → cache →
/// forward) together: the normalized cache key (`twitter:123…`, `pixiv:123`,
/// `bsky:handle/rkey`) instead of the raw URL, so logs stay short and do not
/// echo full user-submitted URLs at info level.
pub fn log_key(url: &str) -> String {
    x_media::site::cache_key(url).unwrap_or_else(|| "<unsupported>".to_string())
}

/// Edit-before-forward: a reply to the prompt swaps the caption of the first
/// forwarded message. Returns true when the message was consumed as an edit.
async fn edit_message_handler(bot: &Bot, message: &Message) -> bool {
    let Some(reply) = message.reply_to_message() else {
        return false;
    };
    let chat_id = message.chat.id.0;
    let Some(text) = message.text() else {
        return false;
    };
    let chat_data = CHAT_STORE.get(chat_id).await;
    let Some(edit) = chat_data.edit_message.get(&(reply.id.0 as i64)) else {
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
    let result = bot
        .edit_message_caption(ChatId(chat_id), MessageId(*first_forward_id as i32))
        .caption(new_text)
        .parse_mode(ParseMode::Html)
        .await;
    match result {
        Ok(_) => log::info!(
            "edit-before-forward: caption swapped on message {first_forward_id} for prompt {}",
            reply.id.0
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
    // Per-request detail: debug only (message text is user data).
    log::debug!(
        "message from {sender} in {} (private={is_private}): {text_preview}",
        message.chat.id
    );
    // URL/edit flows only run in private chats; commands run in any chat.
    if is_private && edit_message_handler(&bot, &message).await {
        return respond(());
    }
    if let Some(text) = message.text()
        && let Ok(command) = Command::parse(text, "")
    {
        log::debug!("command from {}: {text_preview}", message.chat.id);
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
    }
    respond(())
}
