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

    /// A stand-in for `api.telegram.org` for the tests that must drive a real
    /// `Bot` — its request building, the per-chat limiter, the bot-wide budget
    /// — which the scripted mock bypasses entirely. Records every call and
    /// answers the smallest result each method needs.
    pub(crate) mod fake_api {
        use parking_lot::Mutex;
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        pub(crate) struct FakeApi {
            url: url::Url,
            calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
            server: tokio::task::JoinHandle<()>,
        }

        impl FakeApi {
            /// Binds an ephemeral port and serves until dropped.
            pub(crate) async fn start() -> FakeApi {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let calls = Arc::new(Mutex::new(Vec::new()));
                let recorded = Arc::clone(&calls);
                let server = tokio::spawn(async move {
                    while let Ok((mut socket, _)) = listener.accept().await {
                        let recorded = Arc::clone(&recorded);
                        tokio::spawn(async move {
                            let Some((method, body)) = read_request(&mut socket).await else {
                                return;
                            };
                            recorded.lock().push((method.clone(), body));
                            let payload = serde_json::json!({
                                "ok": true,
                                "result": canned_result(&method),
                            })
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                                payload.len(),
                                payload
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.flush().await;
                        });
                    }
                });
                FakeApi {
                    // Trailing slash: teloxide appends `bot<token>/<method>`.
                    url: url::Url::parse(&format!("http://{addr}/")).unwrap(),
                    calls,
                    server,
                }
            }

            /// Where to point a `Bot`: `Bot::new(token).set_api_url(api.url())`.
            pub(crate) fn url(&self) -> url::Url {
                self.url.clone()
            }

            /// Method names in call order.
            pub(crate) fn methods(&self) -> Vec<String> {
                self.calls.lock().iter().map(|(m, _)| m.clone()).collect()
            }

            /// The JSON body of the first call to `method` (`Null` for a body
            /// that is not JSON, i.e. a multipart upload).
            pub(crate) fn body(&self, method: &str) -> serde_json::Value {
                self.calls
                    .lock()
                    .iter()
                    .find(|(m, _)| m == method)
                    .map(|(_, body)| body.clone())
                    .unwrap_or(serde_json::Value::Null)
            }
        }

        impl Drop for FakeApi {
            fn drop(&mut self) {
                self.server.abort();
            }
        }

        /// The smallest result teloxide can deserialize for a method. The names
        /// arrive as the payload type's own — `SendMediaGroup`, not
        /// `sendMediaGroup`: teloxide builds the URL from that, and the Bot API
        /// accepts the spelling.
        fn canned_result(method: &str) -> serde_json::Value {
            match method {
                "CopyMessages" => serde_json::json!([{ "message_id": 11 }]),
                "SendMediaGroup" => serde_json::json!([minimal_message()]),
                "SendMessage" | "SendAnimation" | "EditMessageCaption" => minimal_message(),
                _ => serde_json::Value::Bool(true),
            }
        }

        fn minimal_message() -> serde_json::Value {
            serde_json::json!({
                "message_id": 1,
                "date": 0,
                "chat": { "id": 1, "type": "private" },
            })
        }

        /// One HTTP/1.1 request: the head up to the blank line, then
        /// `content-length` bytes of body — JSON for most methods, multipart
        /// for the media ones (teloxide sends `SendMediaGroup` that way).
        async fn read_request(socket: &mut TcpStream) -> Option<(String, serde_json::Value)> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(headers_end) = find(&buf, b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..headers_end]).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let body_start = headers_end + 4;
                while buf.len() < body_start + length {
                    let n = socket.read(&mut chunk).await.ok()?;
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let method = head
                    .lines()
                    .next()
                    // `POST /bot<token>/<method>`
                    .and_then(|line| line.split(' ').nth(1))
                    .and_then(|path| path.rsplit('/').next())
                    .unwrap_or_default()
                    .to_string();
                let body = parse_body(&buf[body_start..], &head);
                return Some((method, body));
            }
        }

        /// The request body as JSON: either the JSON body itself, or a
        /// multipart form flattened into an object (each part's value parsed as
        /// JSON when it is one, so `media` comes back as its array).
        fn parse_body(body: &[u8], head: &str) -> serde_json::Value {
            let content_type = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-type:"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            let Some(boundary) = content_type
                .split("boundary=")
                .nth(1)
                .map(|b| b.trim().trim_matches('"').to_string())
            else {
                return serde_json::from_slice(body).unwrap_or_default();
            };
            let text = String::from_utf8_lossy(body);
            let mut fields = serde_json::Map::new();
            for part in text.split(&format!("--{boundary}")).skip(1) {
                let Some((part_head, value)) = part.split_once("\r\n\r\n") else {
                    continue;
                };
                let Some(name) = part_head
                    .split("name=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                else {
                    continue;
                };
                let value = value.trim_end_matches("\r\n");
                fields.insert(
                    name.to_string(),
                    serde_json::from_str(value).unwrap_or_else(|_| value.into()),
                );
            }
            serde_json::Value::Object(fields)
        }

        fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        }
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
        /// `(chat, message, text)` of every text rewrite, in order.
        edited_texts: Mutex<Vec<(i64, i64, String)>>,
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
                edited_texts: Mutex::new(Vec::new()),
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

        /// `(chat, message, text)` of every `edit_message_text`, in order.
        pub(crate) fn edited_texts(&self) -> Vec<(i64, i64, String)> {
            self.edited_texts.lock().clone()
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
            items: Vec<InputMedia>,
        ) -> BoxFuture<'_, Result<Vec<Message>, RequestError>> {
            Box::pin(async move {
                // Record the captions exactly as Telegram receives them (only
                // the first item of a group carries one), so tests can assert
                // what a recipient sees.
                self.captions
                    .lock()
                    .extend(items.iter().filter_map(|item| match item {
                        InputMedia::Photo(photo) => photo.caption.clone(),
                        InputMedia::Video(video) => video.caption.clone(),
                        InputMedia::Animation(animation) => animation.caption.clone(),
                        _ => None,
                    }));
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

        fn edit_message_text(
            &self,
            chat_id: ChatId,
            message_id: MessageId,
            text: String,
        ) -> BoxFuture<'_, Result<(), RequestError>> {
            // Always succeeds: the only caller is the expiry sweep, which
            // tolerates a failure (a prompt the user already deleted), so the
            // script stays free for the call the test is about.
            Box::pin(async move {
                self.calls.lock().push("edit_message_text");
                self.edited_texts
                    .lock()
                    .push((chat_id.0, message_id.0 as i64, text));
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
