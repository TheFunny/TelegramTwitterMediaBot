//! Test support: a scripted [`MediaSender`] mock (no Telegram API involved)
//! and the stand-in Telegram API the real-`Bot` tests talk to.
//!
//! [`MediaSender`]: super::MediaSender

use super::*;
use parking_lot::Mutex;
use teloxide::types::InlineQueryResult;

/// One scripted outcome, consumed front-to-back; the last entry repeats
/// for further calls of the same method kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    GroupOk,
    GroupErr,
    AnimationOk,
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
    /// What each `send_animation` handed Telegram: a URL or a file id as that
    /// string, an upload as `attach://<id>`.
    animation_files: Mutex<Vec<String>>,
    /// What every `answer_inline_query` answered with, one entry per result:
    /// `cached_photo:<file id>`, `photo:<url>`, and so on. An answer with no
    /// results is recorded as an empty inner vec.
    inline_answers: Mutex<Vec<Vec<String>>>,
    /// Builds the error every `*Err` outcome returns (RequestError is not
    /// cloneable, so the factory recreates it per call).
    error: Box<dyn Fn() -> RequestError + Send + Sync>,
}

/// A one-string description of an inline result: the kind plus the file id it
/// is served from, or the URL it points Telegram at.
fn inline_result_tag(result: &InlineQueryResult) -> String {
    match result {
        InlineQueryResult::CachedPhoto(r) => format!("cached_photo:{}", r.photo_file_id.0),
        InlineQueryResult::CachedVideo(r) => format!("cached_video:{}", r.video_file_id.0),
        InlineQueryResult::CachedMpeg4Gif(r) => format!("cached_gif:{}", r.mpeg4_file_id.0),
        InlineQueryResult::Photo(r) => format!("photo:{}", r.photo_url),
        InlineQueryResult::Video(r) => format!("video:{}", r.video_url),
        InlineQueryResult::Mpeg4Gif(r) => format!("gif:{}", r.mpeg4_url),
        other => format!("{other:?}"),
    }
}

/// The smallest `Message` the send paths accept, for the outcomes that must
/// report one (`send_animation` reads its id, and its media for the cache).
pub(crate) fn mock_message(id: i64) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": id,
        "date": 0,
        "chat": { "id": 1, "type": "private" },
    }))
    .expect("a minimal message deserializes")
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
            animation_files: Mutex::new(Vec::new()),
            inline_answers: Mutex::new(Vec::new()),
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

    /// What every `send_animation` handed Telegram, in order.
    pub(crate) fn animation_files(&self) -> Vec<String> {
        self.animation_files.lock().clone()
    }

    /// What every `answer_inline_query` answered with, in call order.
    pub(crate) fn inline_answers(&self) -> Vec<Vec<String>> {
        self.inline_answers.lock().clone()
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
        file: InputFile,
    ) -> BoxFuture<'a, Result<Message, RequestError>> {
        // Record what Telegram was handed: a URL or a file id serializes as
        // that string, an upload as `attach://<id>`. Enough to tell a cached
        // send (which must not re-upload) from a fresh one.
        self.animation_files.lock().push(
            serde_json::to_value(&file)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        );
        Box::pin(async move {
            match self.next("send_animation") {
                Outcome::AnimationOk => Ok(mock_message(MockSender::SENT_ID)),
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

    fn send_html_message(
        &self,
        _chat_id: ChatId,
        text: String,
        _reply_to: Option<MessageId>,
    ) -> BoxFuture<'_, Result<i64, RequestError>> {
        Box::pin(async move {
            self.messages.lock().push(text);
            match self.next("send_html_message") {
                Outcome::MessageOk => Ok(MockSender::SENT_ID),
                Outcome::MessageErr => Err(self.error()),
                other => panic!("unexpected outcome {other:?} for send_html_message"),
            }
        })
    }

    fn answer_inline_query(
        &self,
        _id: InlineQueryId,
        results: Vec<InlineQueryResult>,
        cache_time: u32,
    ) -> BoxFuture<'_, Result<(), RequestError>> {
        // Records what the answer was made of, so a test can tell a cached
        // (file-id) result from a URL one. Always succeeds: the debounce's
        // release path is covered by `DebounceStates` directly.
        assert_eq!(
            cache_time, 300,
            "the inline cache window is what the tests pin"
        );
        self.calls.lock().push("answer_inline_query");
        self.inline_answers
            .lock()
            .push(results.iter().map(inline_result_tag).collect());
        Box::pin(async move { Ok(()) })
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
