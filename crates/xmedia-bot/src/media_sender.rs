//! Send abstraction: the message-sending surface [`send`](crate::send)
//! needs, so the send pipeline can be tested with a scripted mock instead of
//! a live teloxide `Bot`.

use std::future::Future;
use std::pin::Pin;
use teloxide::RequestError;
use teloxide::prelude::Requester;
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, ChatId, InlineKeyboardMarkup, InputFile, InputMedia, Message, MessageId, ParseMode,
    ReplyParameters,
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
    /// attaching `reply_markup`.
    fn send_message(
        &self,
        chat_id: ChatId,
        text: String,
        reply_to: Option<MessageId>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> BoxFuture<'_, Result<Message, RequestError>>;

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
        Box::pin(async move { <Bot as Requester>::copy_messages(self, to, from, ids).await })
    }

    fn send_message(
        &self,
        chat_id: ChatId,
        text: String,
        reply_to: Option<MessageId>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> BoxFuture<'_, Result<Message, RequestError>> {
        Box::pin(async move {
            let mut request = <Bot as Requester>::send_message(self, chat_id, text);
            if let Some(reply_to) = reply_to {
                request = request
                    .reply_parameters(ReplyParameters::new(reply_to).allow_sending_without_reply());
            }
            if let Some(markup) = reply_markup {
                request = request.reply_markup(markup);
            }
            request.await
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
    use std::sync::Mutex;

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
    }

    /// Replays a script and records the method names that were called.
    pub(crate) struct MockSender {
        script: Mutex<Vec<Outcome>>,
        cursor: Mutex<usize>,
        calls: Mutex<Vec<&'static str>>,
        /// Builds the error every `*Err` outcome returns (RequestError is not
        /// cloneable, so the factory recreates it per call).
        error: Box<dyn Fn() -> RequestError + Send + Sync>,
    }

    impl MockSender {
        pub(crate) fn scripted(
            script: Vec<Outcome>,
            error: impl Fn() -> RequestError + Send + Sync + 'static,
        ) -> Self {
            MockSender {
                script: Mutex::new(script),
                cursor: Mutex::new(0),
                calls: Mutex::new(Vec::new()),
                error: Box::new(error),
            }
        }

        /// Method names in call order (e.g. `["send_media_group",
        /// "send_media_group"]` proves the fallback re-sent).
        pub(crate) fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn next(&self, kind: &'static str) -> Outcome {
            self.calls.lock().unwrap().push(kind);
            let script = self.script.lock().unwrap();
            let mut cursor = self.cursor.lock().unwrap();
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
            _text: String,
            _reply_to: Option<MessageId>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> BoxFuture<'_, Result<Message, RequestError>> {
            Box::pin(async move {
                match self.next("send_message") {
                    Outcome::MessageErr => Err(self.error()),
                    other => panic!("unexpected outcome {other:?} for send_message"),
                }
            })
        }

        fn send_chat_action(
            &self,
            _chat_id: ChatId,
            _action: ChatAction,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push("send_chat_action");
                Ok(())
            })
        }
    }
}
