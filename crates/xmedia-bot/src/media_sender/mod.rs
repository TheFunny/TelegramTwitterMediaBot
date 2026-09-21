//! Send abstraction: the message-sending surface [`send`](crate::send)
//! needs, so the send pipeline can be tested with a scripted mock instead of
//! a live teloxide `Bot`.

use std::future::Future;
use std::pin::Pin;
use teloxide::RequestError;
use teloxide::prelude::Requester;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQueryId, ChatAction, ChatId, InlineKeyboardMarkup, InputFile, InputMedia, Message,
    MessageId, ParseMode, ReplyParameters,
};

/// Boxed, `Send` future returned by a [`MediaSender`] method (`async fn` in
/// traits is not dyn-compatible).
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The message-sending surface the send pipeline uses. The production
/// implementation is teloxide's [`Bot`]; tests inject a scripted mock to
/// cover the fallback and classification logic without touching the
/// Telegram API.
pub trait MediaSender: Send + Sync {
    /// Sends a media group, replying to `reply_to`.
    fn send_media_group(
        &self,
        chat_id: ChatId,
        reply_to: MessageId,
        items: Vec<InputMedia>,
    ) -> BoxFuture<'_, Result<Vec<Message>, RequestError>>;

    /// Sends a lone animation, replying to `reply_to`.
    fn send_animation<'a>(
        &'a self,
        chat_id: ChatId,
        reply_to: MessageId,
        caption: &'a str,
        spoiler: bool,
        file: InputFile,
    ) -> BoxFuture<'a, Result<Message, RequestError>>;

    /// Copies messages between chats (forward to channel).
    fn copy_messages(
        &self,
        to: ChatId,
        from: ChatId,
        ids: Vec<MessageId>,
    ) -> BoxFuture<'_, Result<Vec<MessageId>, RequestError>>;

    /// Sends a plain text message, optionally replying to `reply_to` and
    /// attaching `reply_markup`. Returns the sent message's id: the bot only
    /// ever needs that (the edit-before-forward prompt's record is keyed by
    /// it), and returning the whole `Message` would force every test mock to
    /// construct one.
    fn send_message(
        &self,
        chat_id: ChatId,
        text: String,
        reply_to: Option<MessageId>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> BoxFuture<'_, Result<i64, RequestError>>;

    /// Answers a callback query, optionally with a toast `text` shown to the
    /// user who pressed the button.
    fn answer_callback_query(
        &self,
        id: CallbackQueryId,
        text: Option<String>,
    ) -> BoxFuture<'_, Result<(), RequestError>>;

    /// Rewrites a message's text and drops its inline keyboard: the
    /// edit-expiry sweep rewriting a prompt whose record expired (a button left
    /// behind could only answer "Expired").
    fn edit_message_text(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: String,
    ) -> BoxFuture<'_, Result<(), RequestError>>;

    /// Rewrites a message's caption, always with HTML parse mode (every caller
    /// in this bot renders escaped HTML: templates and edit-before-forward
    /// links).
    fn edit_message_caption(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        caption: String,
    ) -> BoxFuture<'_, Result<(), RequestError>>;

    /// Deletes a message (the edit-before-forward prompt after a forward).
    fn delete_message(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> BoxFuture<'_, Result<(), RequestError>>;

    /// Sets the chat's "typing / uploading …" indicator (cosmetic).
    fn send_chat_action(
        &self,
        chat_id: ChatId,
        action: ChatAction,
    ) -> BoxFuture<'_, Result<(), RequestError>>;
}

impl MediaSender for Bot {
    fn send_media_group(
        &self,
        chat_id: ChatId,
        reply_to: MessageId,
        items: Vec<InputMedia>,
    ) -> BoxFuture<'_, Result<Vec<Message>, RequestError>> {
        Box::pin(async move {
            // Pace media sends per chat (one token per item) so bursts do not
            // trip Telegram's flood control.
            crate::rate_limit::limiter_for(chat_id.0)
                .acquire(items.len() as f64)
                .await;
            // Same spend against the bot-wide budget: a fan-out over chats is
            // invisible to the per-chat buckets.
            crate::rate_limit::acquire_global(items.len() as f64).await;
            // `<Bot as Requester>::` disambiguates from this trait's same-named
            // method (teloxide's API lives in the `Requester` trait).
            <Bot as Requester>::send_media_group(self, chat_id, items)
                .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply())
                .await
        })
    }

    fn send_animation<'a>(
        &'a self,
        chat_id: ChatId,
        reply_to: MessageId,
        caption: &'a str,
        spoiler: bool,
        file: InputFile,
    ) -> BoxFuture<'a, Result<Message, RequestError>> {
        Box::pin(async move {
            crate::rate_limit::limiter_for(chat_id.0).acquire(1.0).await;
            crate::rate_limit::acquire_global(1.0).await;
            let mut request = <Bot as Requester>::send_animation(self, chat_id, file)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply());
            if spoiler {
                request = request.has_spoiler(true);
            }
            request.await
        })
    }

    fn copy_messages(
        &self,
        to: ChatId,
        from: ChatId,
        ids: Vec<MessageId>,
    ) -> BoxFuture<'_, Result<Vec<MessageId>, RequestError>> {
        Box::pin(async move {
            // Channel forwards are the burstiest path (batch copies); pace
            // them per message against the channel's budget.
            crate::rate_limit::limiter_for(to.0)
                .acquire(ids.len() as f64)
                .await;
            crate::rate_limit::acquire_global(ids.len() as f64).await;
            <Bot as Requester>::copy_messages(self, to, from, ids).await
        })
    }

    fn send_message(
        &self,
        chat_id: ChatId,
        text: String,
        reply_to: Option<MessageId>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> BoxFuture<'_, Result<i64, RequestError>> {
        Box::pin(async move {
            let mut request = <Bot as Requester>::send_message(self, chat_id, text);
            if let Some(reply_to) = reply_to {
                request = request
                    .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply());
            }
            if let Some(markup) = reply_markup {
                request = request.reply_markup(markup);
            }
            request.await.map(|message| message.id.0 as i64)
        })
    }

    fn answer_callback_query(
        &self,
        id: CallbackQueryId,
        text: Option<String>,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        Box::pin(async move {
            let mut request = <Bot as Requester>::answer_callback_query(self, id);
            if let Some(text) = text {
                request = request.text(text);
            }
            request.await.map(|_| ())
        })
    }

    fn edit_message_text(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: String,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        Box::pin(async move {
            <Bot as Requester>::edit_message_text(self, chat_id, message_id, text)
                .reply_markup(InlineKeyboardMarkup::default())
                .await
                .map(|_| ())
        })
    }

    fn edit_message_caption(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        caption: String,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        Box::pin(async move {
            <Bot as Requester>::edit_message_caption(self, chat_id, message_id)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await
                .map(|_| ())
        })
    }

    fn delete_message(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        Box::pin(async move {
            <Bot as Requester>::delete_message(self, chat_id, message_id)
                .await
                .map(|_| ())
        })
    }

    fn send_chat_action(
        &self,
        chat_id: ChatId,
        action: ChatAction,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        Box::pin(async move {
            // teloxide's `send_chat_action` returns `Result<True, _>` (its
            // unit marker type); map the success to `()`.
            <Bot as Requester>::send_chat_action(self, chat_id, action)
                .await
                .map(|_| ())
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support;
