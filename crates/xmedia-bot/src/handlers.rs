use crate::config::Config;
use crate::db::now_f64;
use crate::link_cache::{CachedMediaKind, CachedPost, LinkCache};
use crate::queue::PersistentTaskQueue;
use crate::send::{self, MediaItemPayload, Task};
use crate::state::{ChatData, ChatStore, unix_now};
use std::collections::HashSet;
use std::sync::LazyLock;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatAction, ChatId, ChatKind, InlineQuery, InlineQueryResult,
    InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, Message,
    MessageEntityKind, MessageId, ParseMode, Recipient, ReplyParameters,
};
use teloxide::utils::command::BotCommands;
use x_media::media::Media;

/// One URL job: bot handle + the message + the extracted URL.
type UrlJob = (Bot, Message, String);
/// Bounded channel of URL jobs drained by [`start_url_workers`]. The bound
/// caps both queued memory and shutdown backlog; a full channel applies
/// backpressure to the per-chat handler instead of spawning unbounded tasks.
static URL_JOBS: LazyLock<parking_lot::Mutex<Option<tokio::sync::mpsc::Sender<UrlJob>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));
/// Set by main's shutdown sequence; workers stop pulling new jobs.
static URL_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Worker count draining URL jobs; keeps the old 8-permit concurrency cap
/// while bounding how many jobs can be queued at all.
const URL_WORKERS: usize = 8;

/// Starts the URL job workers (called once from main after the queue starts).
/// teloxide dispatches updates to a per-chat worker that handles them
/// sequentially, so a batch-forward of many messages would otherwise be
/// processed one at a time (fetch + send each, roughly a second per
/// message); the workers add throughput, and FIFO order preserves per-message
/// URL order.
pub async fn start_url_workers() {
    let (tx, rx) = tokio::sync::mpsc::channel::<UrlJob>(256);
    *URL_JOBS.lock() = Some(tx);
    let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
    for _ in 0..URL_WORKERS {
        let rx = std::sync::Arc::clone(&rx);
        tokio::spawn(async move {
            while !URL_STOP.load(std::sync::atomic::Ordering::Relaxed) {
                let job = rx.lock().await.recv().await;
                match job {
                    Some((bot, message, url)) => url_media(bot, &message, &url).await,
                    None => break,
                }
            }
        });
    }
}

/// Stops URL workers (drains up to the 256 queued jobs, then exits).
pub fn stop_url_workers() {
    URL_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub static CHAT_STORE: LazyLock<ChatStore> =
    LazyLock::new(|| ChatStore::open("data/task_queue.db").expect("failed to open chat store"));
pub static TASK_QUEUE: LazyLock<PersistentTaskQueue> =
    LazyLock::new(|| PersistentTaskQueue::new("data/task_queue.db"));
pub static LINK_CACHE: LazyLock<LinkCache> =
    LazyLock::new(|| LinkCache::open("data/task_queue.db"));
pub static CONFIG: LazyLock<Config> = LazyLock::new(Config::load);

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "snake_case",
    description = "Turn X/Pixiv/Bluesky links into media messages"
)]
enum Command {
    #[command(description = "Get started")]
    Start,
    #[command(description = "Show command help")]
    Help,
    #[command(
        description = "Set forward channel (@channel or ID)",
        parse_with = "split"
    )]
    SetForwardChannel(String),
    #[command(description = "Remove forward channel")]
    RemoveForwardChannel,
    #[command(description = "Toggle edit-before-forward")]
    EditBeforeForward,
    #[command(
        description = "Reply with [] to save as template",
        parse_with = "split"
    )]
    SetTemplate(String),
    #[command(description = "Show chat state (debug)")]
    BotDict,
    #[command(description = "Set site caption format", parse_with = "split")]
    SetFormat(String),
    #[command(
        description = "Clear link cache (admin; optional URL, else all)",
        parse_with = "split"
    )]
    ClearCache(String),
}

async fn reply<T>(bot: Bot, message: Message, text: T) -> Result<Message, RequestError>
where
    T: Into<String>,
{
    bot.send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id).allow_sending_without_reply())
        .await
}

/// Extracts URL and text-link entities (text + caption), deduped in order.
pub fn extract_urls(message: &Message) -> Vec<String> {
    let mut urls = Vec::new();
    for entity in message.parse_entities().into_iter().flatten() {
        match entity.kind() {
            MessageEntityKind::Url => urls.push(entity.text().to_string()),
            MessageEntityKind::TextLink { url } => urls.push(url.to_string()),
            _ => {}
        }
    }
    for entity in message.parse_caption_entities().into_iter().flatten() {
        match entity.kind() {
            MessageEntityKind::Url => urls.push(entity.text().to_string()),
            MessageEntityKind::TextLink { url } => urls.push(url.to_string()),
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    // Dedup by the normalized post id so variant URLs of the same post
    // (/status/1 vs /status/1/photo/1) are sent once; unsupported URLs fall
    // back to exact-string dedup.
    urls.retain(|url| seen.insert(x_media::site::cache_key(url).unwrap_or_else(|| url.clone())));
    urls
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

enum SetForwardChannelError {
    EmptyParameter,
    NotChannel,
    NotAdmin,
    NotBotAdmin(RequestError),
    NotBotCanPost,
}

async fn set_forward_channel_handler(
    bot: &Bot,
    message: &Message,
    channel: String,
) -> Result<i64, SetForwardChannelError> {
    if channel.is_empty() {
        return Err(SetForwardChannelError::EmptyParameter);
    }
    let channel = match channel.parse::<i64>() {
        Ok(id) => Recipient::Id(ChatId(id)),
        Err(_) => Recipient::ChannelUsername(channel),
    };
    if let Some(from) = &message.from {
        log::info!(
            "Set forward channel for {} ({}) to {}",
            from.full_name(),
            message.chat.id,
            channel
        );
    }
    let chat = match bot.get_chat(channel.clone()).await {
        Err(e) => {
            log::error!("Failed to get channel {}: {}", channel, e);
            return Err(SetForwardChannelError::NotBotAdmin(e));
        }
        Ok(chat) => chat,
    };
    if !chat.is_channel() {
        return Err(SetForwardChannelError::NotChannel);
    }
    let channel_id = chat.id.0;
    // The sender must be a channel administrator. Compare against the
    // sender's user id, NOT the chat id (they only coincide in private
    // chats, so the old check broke group usage).
    let Some(sender) = message.from.as_ref() else {
        return Err(SetForwardChannelError::NotAdmin);
    };
    match bot.get_chat_administrators(channel.clone()).await {
        Err(e) => {
            log::error!("Failed to get channel administrators {}: {}", channel, e);
            return Err(SetForwardChannelError::NotBotAdmin(e));
        }
        Ok(admins) => {
            if !admins.iter().any(|admin| admin.user.id == sender.id) {
                return Err(SetForwardChannelError::NotAdmin);
            }
            // The bot itself must be an admin that can post; a missing
            // bot entry must not pass silently (copy would fail later).
            let bot_id = match bot.get_me().await {
                Ok(me) => me.user.id,
                Err(e) => return Err(SetForwardChannelError::NotBotAdmin(e)),
            };
            let bot_ok = admins
                .iter()
                .any(|admin| admin.user.id == bot_id && admin.can_post_messages());
            if !bot_ok {
                return Err(SetForwardChannelError::NotBotCanPost);
            }
        }
    }
    Ok(channel_id)
}

async fn execute_command(
    bot: &Bot,
    message: &Message,
    command: Command,
) -> Result<(), RequestError> {
    match command {
        Command::Start => {
            bot.send_message(message.chat.id, "Hello!").await?;
        }
        Command::Help => {
            bot.send_message(message.chat.id, Command::descriptions().to_string())
                .await?;
        }
        Command::SetForwardChannel(channel) => {
            let result = match set_forward_channel_handler(bot, message, channel).await {
                Ok(channel_id) => {
                    CHAT_STORE
                        .update(message.chat.id.0, |data| {
                            data.forward_channel_id = Some(channel_id);
                        })
                        .await;
                    "Add successfully.".to_string()
                }
                Err(SetForwardChannelError::EmptyParameter) => {
                    "Receive empty parameter.\nYou should enter a channel id or username"
                        .to_string()
                }
                Err(SetForwardChannelError::NotChannel) => {
                    "Given id / username is not a channel".to_string()
                }
                Err(SetForwardChannelError::NotAdmin) => {
                    "You are not an administrator of the channel".to_string()
                }
                Err(SetForwardChannelError::NotBotAdmin(e)) => {
                    e.to_string() + "\nPlease add the bot as an admin to the channel"
                }
                Err(SetForwardChannelError::NotBotCanPost) => {
                    "Bot can't post messages to the channel".to_string()
                }
            };
            reply(bot.clone(), message.clone(), result).await?;
        }
        Command::RemoveForwardChannel => {
            let chat_id = message.chat.id.0;
            let text = CHAT_STORE
                .update(chat_id, |data| {
                    if data.forward_channel_id.is_some() {
                        data.forward_channel_id = None;
                        "Remove successfully.".to_string()
                    } else {
                        "No channel to remove.".to_string()
                    }
                })
                .await;
            reply(bot.clone(), message.clone(), text).await?;
        }
        Command::EditBeforeForward => {
            let chat_id = message.chat.id.0;
            let text = CHAT_STORE
                .update(chat_id, |data| {
                    if data.forward_channel_id.is_none() {
                        "Please enable forward channel first.".to_string()
                    } else if data.edit_before_forward {
                        data.edit_before_forward = false;
                        data.edit_message.clear();
                        "Disable edit before forward.".to_string()
                    } else {
                        data.edit_before_forward = true;
                        "Enable edit before forward.".to_string()
                    }
                })
                .await;
            reply(bot.clone(), message.clone(), text).await?;
        }
        Command::SetTemplate(name) => {
            let chat_id = message.chat.id.0;
            let text = match message.reply_to_message() {
                None => "Please reply to a message to set as template.".to_string(),
                Some(reply) => {
                    let reply_text = reply.text().unwrap_or_default();
                    if !reply_text.contains("[]") {
                        "Please reply to a message with [] to set as template.".to_string()
                    } else if name.is_empty() {
                        "Please provide a name for the template.".to_string()
                    } else {
                        CHAT_STORE
                            .update(chat_id, |data| {
                                data.template.insert(
                                    name,
                                    html_escape::encode_text(reply_text).into_owned(),
                                );
                            })
                            .await;
                        "Template set.".to_string()
                    }
                }
            };
            reply(bot.clone(), message.clone(), text).await?;
        }
        Command::BotDict => {
            let chat_data = CHAT_STORE.get(message.chat.id.0).await;
            let debug = format!("{chat_data:?}");
            let text = html_escape::encode_text(&debug).into_owned();
            reply(bot.clone(), message.clone(), text).await?;
        }
        Command::SetFormat(arg) => {
            let chat_id = message.chat.id.0;
            let (site, format) = match arg.split_once(char::is_whitespace) {
                Some((site, format)) if !format.trim().is_empty() => {
                    (site.trim(), format.trim().to_string())
                }
                _ => {
                    reply(
                        bot.clone(),
                        message.clone(),
                        "Usage: /set_format <site> <format>",
                    )
                    .await?;
                    return Ok(());
                }
            };
            if !["twitter", "bsky", "pixiv"].contains(&site) {
                reply(
                    bot.clone(),
                    message.clone(),
                    "Unknown site. Use twitter, bsky or pixiv.",
                )
                .await?;
                return Ok(());
            }
            CHAT_STORE
                .update(chat_id, |data| {
                    data.message_format.insert(site.to_string(), format);
                })
                .await;
            reply(bot.clone(), message.clone(), "Format set.").await?;
        }
        Command::ClearCache(arg) => {
            let sender_id = message
                .from
                .as_ref()
                .map(|user| user.id.0 as i64)
                .unwrap_or(-1);
            if !CONFIG.admin_ids.contains(&sender_id) {
                reply(bot.clone(), message.clone(), "Admin only.").await?;
                return Ok(());
            }
            let arg = arg.trim();
            if arg.is_empty() {
                let removed = LINK_CACHE.clear(None).await;
                log::info!("cache cleared by {sender_id}: {removed} entries");
                reply(
                    bot.clone(),
                    message.clone(),
                    format!("Cleared {removed} cached entr{}.", plural(removed)),
                )
                .await?;
            } else {
                let key = match x_media::site::cache_key(arg) {
                    Some(key) => key,
                    None => {
                        reply(
                            bot.clone(),
                            message.clone(),
                            "Unrecognized link. Use a twitter/x, pixiv or bsky post URL.",
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let removed = LINK_CACHE.clear(Some(&key)).await;
                log::info!("cache entry cleared by {sender_id}: {key} ({removed} rows)");
                reply(
                    bot.clone(),
                    message.clone(),
                    format!(
                        "Cleared cache for {arg} ({} entr{}).",
                        removed,
                        plural(removed)
                    ),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// `"y"` for one, `"ies"` for anything else — "1 entry" / "2 entries".
fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

/// Registers the bot's command list with Telegram so clients show it in the
/// `/` menu (Bot API `setMyCommands`).
pub async fn register_commands(bot: &Bot) -> Result<(), RequestError> {
    let commands = Command::bot_commands();
    bot.set_my_commands(commands.clone()).await?;
    log::info!("registered {} commands", commands.len());
    Ok(())
}

/// For locally produced media (encoded ugoira MP4) the thumbnail URL is a
/// hotlink-protected remote URL Telegram may not fetch; let Telegram generate
/// its own thumbnail instead.
fn thumbnail_for(media: &Media) -> Option<String> {
    let url = media.url();
    if url.starts_with("http://") || url.starts_with("https://") {
        media.thumbnail_url().map(str::to_string)
    } else {
        None
    }
}

fn media_to_payload(media: &Media, sensitive: bool) -> MediaItemPayload {
    let fallback_url = media.smaller_url().map(str::to_string);
    match media {
        // A gif inside a group becomes a video item; a lone gif takes the
        // animation path (see url_media).
        Media::Illustration { .. } => MediaItemPayload::Photo {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            fallback_url,
            file_id: false,
        },
        Media::Video { .. } => MediaItemPayload::Video {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            thumbnail: thumbnail_for(media),
            fallback_url,
            file_id: false,
        },
        Media::Animated { .. } => MediaItemPayload::Video {
            media: media.url().to_string(),
            has_spoiler: sensitive,
            thumbnail: thumbnail_for(media),
            fallback_url,
            file_id: false,
        },
    }
}

async fn enqueue_retry(task: Task, delay_seconds: f64) {
    let payload = serde_json::to_value(task).expect("task serializes");
    let run_after = now_f64() + delay_seconds;
    if let Err(e) = TASK_QUEUE.enqueue(payload, run_after).await {
        log::error!("failed to enqueue retry: {e}");
    }
}

/// Sends a task and handles the outcome: post-send actions on success, retry
/// enqueue on retryable failure, reply + link-cache invalidation on
/// permanent failure (a stale cached file id must not repeat forever).
async fn dispatch_send(bot: Bot, message: &Message, task: &Task, url: &str) {
    let result = match task {
        Task::SendAnimation { .. } => send::send_animation(&bot, task).await,
        Task::SendMediaSequence { .. } => send::send_media_sequence(&bot, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    };
    match result {
        Ok(message_ids) => {
            log::info!("sent {} message(s) for {url}", message_ids.len());
            send::post_send_actions(&bot, task, message_ids).await;
            // The task settled: drop any keep-alive temp media.
            send::release_keep_alive(task);
        }
        Err(send::SendError::Retryable {
            delay_seconds,
            task,
        }) => {
            log::info!("send for {url} failed, queued for retry in {delay_seconds:.1}s");
            enqueue_retry(task, delay_seconds).await;
            let _ = reply(bot, message.clone(), "Send failed. Task queued for retry.").await;
        }
        Err(send::SendError::Permanent {
            message: err_message,
            task,
        }) => {
            send::invalidate_cache(&task).await;
            send::release_keep_alive(&task);
            log::error!("send for {url} failed permanently: {err_message}");
            let _ = reply(bot, message.clone(), format!("Send failed: {err_message}")).await;
        }
    }
}

/// Builds the send task from ready-made items, sharing the payload shape
/// between the fresh-fetch and link-cache paths.
#[allow(clippy::too_many_arguments)]
fn build_send_task(
    chat_data: &ChatData,
    message: &Message,
    source_url: String,
    caption: String,
    items: Vec<MediaItemPayload>,
    cache_data: Option<CachedPost>,
) -> Task {
    let chat_id = message.chat.id.0;
    if items.len() == 1 && matches!(items[0], MediaItemPayload::Animation { .. }) {
        Task::SendAnimation {
            chat_id,
            reply_to_message_id: message.id.0 as i64,
            caption,
            animation: items.into_iter().next().unwrap(),
            source_url,
            edit_before_forward: chat_data.edit_before_forward,
            forward_channel_id: chat_data.forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(message.id.0 as i64),
            cache_data,
        }
    } else {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id: message.id.0 as i64,
            caption,
            media_batches: send::chunk_media_items(items),
            batch_index: 0,
            sent_message_ids: vec![],
            source_url,
            edit_before_forward: chat_data.edit_before_forward,
            forward_channel_id: chat_data.forward_channel_id,
            notify_chat_id: Some(chat_id),
            notify_message_id: Some(message.id.0 as i64),
            cache_data,
        }
    }
}

async fn url_media(bot: Bot, message: &Message, url: &str) {
    let chat_id = message.chat.id.0;
    if let Err(e) = bot
        .send_chat_action(ChatId(chat_id), ChatAction::Typing)
        .await
    {
        log::error!("send_chat_action failed: {e}");
    }

    // Link cache: a post sent before is re-sent from Telegram file ids —
    // no source-site request, no download, no upload. Keyed by the
    // normalized post id so x.com / fxtwitter / /photo/N variants collide.
    if let Some(key) = x_media::site::cache_key(url)
        && let Some(cached) = LINK_CACHE.get(&key, CONFIG.link_cache_ttl).await
    {
        log::info!("link cache hit for {url}");
        let chat_data = CHAT_STORE.get(chat_id).await;
        let site = key.split(':').next().unwrap_or("unknown");
        let format = chat_data
            .message_format
            .get(site)
            .cloned()
            .unwrap_or_default();
        let caption = if format.is_empty() {
            cached.caption.clone()
        } else {
            x_media::site::caption_from_fields(
                &format,
                "",
                &cached.url,
                &cached.author,
                &cached.author_url,
                &cached.title,
                &cached.tags,
            )
        };
        let items: Vec<MediaItemPayload> = cached
            .media
            .iter()
            .map(|m| match m.kind {
                CachedMediaKind::Photo => MediaItemPayload::Photo {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    fallback_url: None,
                    file_id: true,
                },
                CachedMediaKind::Video => MediaItemPayload::Video {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    thumbnail: None,
                    fallback_url: None,
                    file_id: true,
                },
                CachedMediaKind::Animation => MediaItemPayload::Animation {
                    media: m.file_id.clone(),
                    has_spoiler: cached.sensitive,
                    file_id: true,
                },
            })
            .collect();
        let task = build_send_task(
            &chat_data,
            message,
            cached.url.clone(),
            caption,
            items,
            Some(cached),
        );
        dispatch_send(bot, message, &task, url).await;
        return;
    }

    log::info!("fetching {url}");
    match x_media::site::fetch(url).await {
        // Unsupported links are ignored silently (Python parity).
        Ok(None) => {
            log::info!("no site pattern matches {url}; ignoring");
        }
        // Retries exhausted: notify the user (Rust-only requirement 3).
        Err(e) => {
            log::error!("fetch {url}: {e}");
            let _ = reply(
                bot,
                message.clone(),
                "Failed to fetch media from this link.",
            )
            .await;
        }
        Ok(Some(mut fetched)) => {
            if fetched.media.is_empty() {
                let _ = reply(
                    bot,
                    message.clone(),
                    "No media found or media type is not supported.",
                )
                .await;
                return;
            }
            let chat_data = CHAT_STORE.get(chat_id).await;
            // Per-site caption format override (empty -> built-in caption).
            let format = chat_data
                .message_format
                .get(fetched.site_name())
                .cloned()
                .unwrap_or_default();
            let caption = fetched.caption_with(&format);
            // Raw render data for the link cache; the send fills in the
            // Telegram file ids and persists the entry.
            let cache_data = fetched
                .render_fields()
                .map(|(author, author_url, title, tags)| CachedPost {
                    url: fetched.source_url.clone(),
                    caption: fetched.caption.clone(),
                    title: title.to_string(),
                    author: author.to_string(),
                    author_url: author_url.to_string(),
                    tags: tags.to_string(),
                    sensitive: fetched.sensitive,
                    media: vec![],
                });
            let items: Vec<MediaItemPayload> = fetched
                .media
                .iter()
                .map(|media| media_to_payload(media, fetched.sensitive))
                .collect();
            let task = build_send_task(
                &chat_data,
                message,
                fetched.source_url.clone(),
                caption,
                items,
                cache_data,
            );
            // Hand the keep-alive temp dir (ugoira / bsky remux MP4) to the
            // retry registry: a queued retry runs after this function returns
            // and the fetch's own TempDir is dropped, so without this the
            // local file would be gone by the time the retry sends it.
            if let Some(dir) = fetched.take_keep_alive() {
                send::KEEP_ALIVE.lock().push(dir);
            }
            dispatch_send(bot, message, &task, url).await;
        }
    }
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
    log::info!(
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
        log::info!("command from {}: {text_preview}", message.chat.id);
        execute_command(&bot, &message, command).await?;
        return respond(());
    }
    if is_private {
        let urls = extract_urls(&message);
        if !urls.is_empty() {
            log::info!("extracted {} URL(s): {urls:?}", urls.len());
        }
        for url in urls {
            // Clone out of the lock: the parking_lot guard is !Send and must
            // not be held across the await below.
            let Some(tx) = URL_JOBS.lock().clone() else {
                log::warn!("url workers not started; dropping link");
                break;
            };
            let _ = tx.send((bot.clone(), message.clone(), url)).await;
        }
    }
    respond(())
}

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
    log::info!("inline query: {}", query.query);
    match x_media::site::fetch(&query.query).await {
        Ok(Some(fetched)) => {
            let mut results: Vec<InlineQueryResult> = Vec::new();
            for (i, media) in fetched.media.iter().enumerate() {
                let id = format!("{i}");
                let Some(url) = url::Url::parse(media.url()).ok() else {
                    continue;
                };
                let thumbnail = media
                    .thumbnail_url()
                    .and_then(|t| url::Url::parse(t).ok())
                    .unwrap_or_else(|| url.clone());
                let caption = fetched.caption.clone();
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

pub async fn callback_query_handler(bot: Bot, query: CallbackQuery) -> Result<(), RequestError> {
    let callback_query_id = query.id;
    let data = query.data.clone();
    let Some(message) = &query.message else {
        return respond(());
    };
    let chat_id = message.chat().id.0;
    let prompt_message_id = message.id().0 as i64;
    let ttl_secs = CONFIG.edit_message_ttl.as_secs() as i64;
    let chat_data = CHAT_STORE.get(chat_id).await;
    let edit = chat_data.edit_message.get(&prompt_message_id).cloned();
    let Some(edit) = edit else {
        log::info!(
            "callback from {}: no edit record for prompt {prompt_message_id}",
            chat_id
        );
        bot.answer_callback_query(callback_query_id)
            .text("Expired")
            .await?;
        return respond(());
    };
    // Lazy expiry: a stale record (past the TTL, not yet swept) is dropped.
    if edit.created_at + ttl_secs <= unix_now() {
        CHAT_STORE
            .update(chat_id, |data| {
                data.edit_message.remove(&prompt_message_id);
            })
            .await;
        bot.answer_callback_query(callback_query_id)
            .text("Expired")
            .await?;
        return respond(());
    }

    let Some(data) = data else {
        return respond(());
    };
    log::info!(
        "callback from {} on prompt {prompt_message_id}: {data}",
        chat_id
    );
    if data == "forward" {
        match chat_data.forward_channel_id {
            Some(channel_id) => {
                let forward_task = Task::ForwardMessages {
                    from_chat_id: edit.chat_id,
                    to_chat_id: channel_id,
                    message_ids: edit.forward_message_ids.clone(),
                    notify_chat_id: Some(chat_id),
                    notify_message_id: Some(prompt_message_id),
                };
                match send::forward_messages(&bot, &forward_task).await {
                    Ok(()) => {
                        log::info!(
                            "forwarded {} message(s) to channel {channel_id}",
                            edit.forward_message_ids.len()
                        );
                        bot.answer_callback_query(callback_query_id)
                            .text("✅ Forwarded")
                            .await?;
                        let _ = bot
                            .delete_message(ChatId(chat_id), MessageId(prompt_message_id as i32))
                            .await;
                        CHAT_STORE
                            .update(chat_id, |data| {
                                data.edit_message.remove(&prompt_message_id);
                            })
                            .await;
                    }
                    Err(send::SendError::Retryable {
                        delay_seconds,
                        task,
                    }) => {
                        log::info!("forward queued for retry in {delay_seconds:.1}s");
                        enqueue_retry(task, delay_seconds).await;
                        bot.answer_callback_query(callback_query_id)
                            .text("Forward queued for retry.")
                            .await?;
                    }
                    Err(send::SendError::Permanent { message, .. }) => {
                        log::error!("forward failed permanently: {message}");
                        bot.answer_callback_query(callback_query_id)
                            .text(format!("Forward failed: {message}"))
                            .await?;
                    }
                }
            }
            None => {
                log::info!("forward callback without a forward channel set");
                bot.answer_callback_query(callback_query_id)
                    .text("No forward channel set.")
                    .await?;
            }
        }
        return respond(());
    }
    if let Some(name) = data.strip_prefix("template|") {
        if let Some(template_html) = chat_data.template.get(name).cloned()
            && let Some(first_forward_id) = edit.forward_message_ids.first().copied()
        {
            // Raw template including the [] placeholder (Python parity).
            let _ = bot
                .edit_message_caption(ChatId(chat_id), MessageId(first_forward_id as i32))
                .caption(template_html)
                .parse_mode(ParseMode::Html)
                .await;
            CHAT_STORE
                .update(chat_id, |data| {
                    if let Some(entry) = data.edit_message.get_mut(&prompt_message_id) {
                        entry.template = name.to_string();
                    }
                })
                .await;
            log::info!("template '{name}' applied to prompt {prompt_message_id}");
        }
        bot.answer_callback_query(callback_query_id).await?;
    }
    respond(())
}
