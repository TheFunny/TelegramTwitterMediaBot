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

/// Test support: a scripted [`MediaSender`] mock (no Telegram API involved).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use parking_lot::Mutex;

    /// One scripted outcome, consumed front-to-back; the last entry repeats
    /// for further calls of the same method kind.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Outcome {
        GroupOk,
        GroupErr,
        AnimationErr,
        CopyOk,
        CopyErr,
        /// An error from `send_message` (replies are fire-and-forget, so an
        /// error is fine for tests).
        MessageErr,
        /// A successful `send_message`, returning message id [`MockSender::SENT_ID`].
        MessageOk,
        EditOk,
        EditErr,
    }

    /// Replays a script and records what was sent, so tests can assert the
    /// user-visible text a path produced.
    pub(crate) struct MockSender {
        script: Mutex<Vec<Outcome>>,
        cursor: Mutex<usize>,
        calls: Mutex<Vec<&'static str>>,
        messages: Mutex<Vec<String>>,
        captions: Mutex<Vec<String>>,
        answers: Mutex<Vec<Option<String>>>,
        /// Builds the error every `*Err` outcome returns (RequestError is not
        /// cloneable, so the factory recreates it per call).
        error: Box<dyn Fn() -> RequestError + Send + Sync>,
    }

    impl MockSender {
        /// The message id a successful `send_message` reports.
        pub(crate) const SENT_ID: i64 = 1;

        pub(crate) fn scripted(
            script: Vec<Outcome>,
            error: impl Fn() -> RequestError + Send + Sync + 'static,
        ) -> Self {
            MockSender {
                script: Mutex::new(script),
                cursor: Mutex::new(0),
                calls: Mutex::new(Vec::new()),
                messages: Mutex::new(Vec::new()),
                captions: Mutex::new(Vec::new()),
                answers: Mutex::new(Vec::new()),
                error: Box::new(error),
            }
        }

        /// Method names in call order (e.g. `["send_media_group",
        /// "send_media_group"]` proves the fallback re-sent).
        pub(crate) fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().clone()
        }

        /// Texts of the plain messages sent, in order.
        pub(crate) fn messages(&self) -> Vec<String> {
            self.messages.lock().clone()
        }

        /// Captions passed to `edit_message_caption`, in order.
        pub(crate) fn captions(&self) -> Vec<String> {
            self.captions.lock().clone()
        }

        /// Toast texts of the answered callback queries, in order.
        pub(crate) fn answers(&self) -> Vec<Option<String>> {
            self.answers.lock().clone()
        }

        fn next(&self, kind: &'static str) -> Outcome {
            self.calls.lock().push(kind);
            let script = self.script.lock();
            let mut cursor = self.cursor.lock();
            if script.is_empty() {
                panic!("mock script exhausted: {kind}");
            }
            let idx = (*cursor).min(script.len() - 1);
            *cursor = idx + 1;
            script[idx]
        }

        fn error(&self) -> RequestError {
            (self.error)()
        }
    }

    impl MediaSender for MockSender {
        fn send_media_group(
            &self,
            _chat_id: ChatId,
            _reply_to: MessageId,
            _items: Vec<InputMedia>,
        ) -> BoxFuture<'_, Result<Vec<Message>, RequestError>> {
            Box::pin(async move {
                match self.next("send_media_group") {
                    Outcome::GroupOk => Ok(Vec::new()),
                    Outcome::GroupErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for send_media_group"),
                }
            })
        }

        fn send_animation<'a>(
            &'a self,
            _chat_id: ChatId,
            _reply_to: MessageId,
            _caption: &'a str,
            _spoiler: bool,
            _file: InputFile,
        ) -> BoxFuture<'a, Result<Message, RequestError>> {
            Box::pin(async move {
                match self.next("send_animation") {
                    Outcome::AnimationErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for send_animation"),
                }
            })
        }

        fn copy_messages(
            &self,
            _to: ChatId,
            _from: ChatId,
            _ids: Vec<MessageId>,
        ) -> BoxFuture<'_, Result<Vec<MessageId>, RequestError>> {
            Box::pin(async move {
                match self.next("copy_messages") {
                    Outcome::CopyOk => Ok(vec![MessageId(1)]),
                    Outcome::CopyErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for copy_messages"),
                }
            })
        }

        fn send_message(
            &self,
            _chat_id: ChatId,
            text: String,
            _reply_to: Option<MessageId>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> BoxFuture<'_, Result<i64, RequestError>> {
            Box::pin(async move {
                self.messages.lock().push(text);
                match self.next("send_message") {
                    Outcome::MessageOk => Ok(MockSender::SENT_ID),
                    Outcome::MessageErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for send_message"),
                }
            })
        }

        fn answer_callback_query(
            &self,
            _id: CallbackQueryId,
            text: Option<String>,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            // Always succeeds: the toast is cosmetic, so the script stays
            // focused on the outcomes a test cares about.
            Box::pin(async move {
                self.calls.lock().push("answer_callback_query");
                self.answers.lock().push(text);
                Ok(())
            })
        }

        fn edit_message_caption(
            &self,
            _chat_id: ChatId,
            _message_id: MessageId,
            caption: String,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            Box::pin(async move {
                self.captions.lock().push(caption);
                match self.next("edit_message_caption") {
                    Outcome::EditOk => Ok(()),
                    Outcome::EditErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for edit_message_caption"),
                }
            })
        }

        fn delete_message(
            &self,
            _chat_id: ChatId,
            _message_id: MessageId,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            // Deletion is fire-and-forget in every caller; always succeeds.
            Box::pin(async move {
                self.calls.lock().push("delete_message");
                Ok(())
            })
        }

        fn send_chat_action(
            &self,
            _chat_id: ChatId,
            _action: ChatAction,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            Box::pin(async move {
                self.calls.lock().push("send_chat_action");
                Ok(())
            })
        }
    }
}
