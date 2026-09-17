//! Bot command parsing, the `/`-command executor and `setMyCommands`
//! registration. URL/inline/callback flows live in their own modules.

use super::urls::{PostSend, url_media};
use super::{CHAT_STORE, CONFIG, LINK_CACHE, log_key, reply, reply_html};
use crate::ctx::AppContext;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Message, Recipient};
use teloxide::utils::command::{BotCommands, ParseError};

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "snake_case",
    description = "Turn X/Pixiv/Bluesky links into media messages"
)]
pub(crate) enum Command {
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
    #[command(description = "Show chat state (debug; admin only)")]
    BotDict,
    #[command(description = "Set site caption format", parse_with = "split")]
    SetFormat(String),
    #[command(
        description = "Clear link cache (admin; optional URL, else all)",
        parse_with = "split"
    )]
    ClearCache(String),
    #[command(
        description = "Send a link's media (no forwarding)",
        parse_with = parse_arg_remainder
    )]
    Test(String),
    #[command(
        description = "Parse a link and report it (debug; nothing sent)",
        parse_with = parse_arg_remainder
    )]
    Debug(String),
}

/// `/test` and `/debug` argument parser: the whole remainder after the command
/// name, trimmed. The built-in `split` parser takes exactly one space-separated
/// token and rejects the rest, so a URL followed by a trailing space (or
/// pasted text) would silently fall through to the URL flow instead.
fn parse_arg_remainder(s: String) -> Result<(String,), ParseError> {
    Ok((s.trim().to_string(),))
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

pub(crate) async fn execute_command(
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
            reply(bot, message.chat.id.0, message.id, result).await?;
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
            reply(bot, message.chat.id.0, message.id, text).await?;
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
            reply(bot, message.chat.id.0, message.id, text).await?;
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
            reply(bot, message.chat.id.0, message.id, text).await?;
        }
        Command::BotDict => {
            // Debug dump of the chat's persisted state: admin only (it echoes
            // forward-channel ids and templates to whoever asks).
            let sender_id = message
                .from
                .as_ref()
                .map(|user| user.id.0 as i64)
                .unwrap_or(-1);
            if !CONFIG.admin_ids.contains(&sender_id) {
                reply(bot, message.chat.id.0, message.id, "Admin only.").await?;
                return Ok(());
            }
            let chat_data = CHAT_STORE.get(message.chat.id.0).await;
            let debug = html_escape::encode_text(&format!("{chat_data:?}")).into_owned();
            // A chat with many templates/edit records exceeds Telegram's 4096
            // char message limit; the dump is plain text (no parse mode), so a
            // plain byte-boundary cut is safe.
            let end = debug.floor_char_boundary(MAX_DEBUG_DUMP_CHARS.min(debug.len()));
            let text = if end < debug.len() {
                format!("{}…", &debug[..end])
            } else {
                debug
            };
            reply(bot, message.chat.id.0, message.id, text).await?;
        }
        Command::SetFormat(arg) => {
            let chat_id = message.chat.id.0;
            let (site, format) = match arg.split_once(char::is_whitespace) {
                Some((site, format)) if !format.trim().is_empty() => {
                    (site.trim(), format.trim().to_string())
                }
                _ => {
                    reply(
                        bot,
                        message.chat.id.0,
                        message.id,
                        "Usage: /set_format <site> <format>",
                    )
                    .await?;
                    return Ok(());
                }
            };
            if !x_media::site::site_ids().contains(&site) {
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    "Unknown site. Use twitter, bsky, pixiv, misskey or bilibili.",
                )
                .await?;
                return Ok(());
            }
            CHAT_STORE
                .update(chat_id, |data| {
                    data.message_format.insert(site.to_string(), format);
                })
                .await;
            reply(bot, message.chat.id.0, message.id, "Format set.").await?;
        }
        Command::ClearCache(arg) => {
            let sender_id = message
                .from
                .as_ref()
                .map(|user| user.id.0 as i64)
                .unwrap_or(-1);
            if !CONFIG.admin_ids.contains(&sender_id) {
                reply(bot, message.chat.id.0, message.id, "Admin only.").await?;
                return Ok(());
            }
            let arg = arg.trim();
            if arg.is_empty() {
                let removed = LINK_CACHE.clear(None).await;
                log::info!("cache cleared by {sender_id}: {removed} entries");
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    format!("Cleared {removed} cached entr{}.", plural(removed)),
                )
                .await?;
            } else {
                let key = match x_media::site::cache_key(arg) {
                    Some(key) => key,
                    None => {
                        reply(
                            bot,
                            message.chat.id.0,
                            message.id,
                            "Unrecognized link. Use a twitter/x, pixiv, bsky, misskey or bilibili post URL.",
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let removed = LINK_CACHE.clear(Some(&key)).await;
                log::info!("cache entry cleared by {sender_id}: {key} ({removed} rows)");
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    format!(
                        "Cleared cache for {arg} ({} entr{}).",
                        removed,
                        plural(removed)
                    ),
                )
                .await?;
            }
        }
        Command::Test(arg) => {
            let url = arg.trim();
            if url.is_empty() {
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    "Usage: /test <post url>",
                )
                .await?;
                return Ok(());
            }
            if x_media::site::cache_key(url).is_none() {
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    "No enabled site matches this link (twitter/x, pixiv, bsky, misskey or bilibili).",
                )
                .await?;
                return Ok(());
            }
            // The ordinary link pipeline with the chat's post-send actions
            // suppressed: the media is sent (and cached) like a normal link,
            // but nothing is forwarded to the channel and no
            // edit-before-forward prompt opens. Info level echoes the
            // normalized key (never the raw URL) per the logging convention.
            log::info!("test: sending [key={}]", log_key(url));
            let ctx = AppContext::from_statics(bot);
            url_media(
                &ctx,
                message.chat.id.0,
                message.id.0 as i64,
                url,
                PostSend::Suppressed,
            )
            .await;
        }
        Command::Debug(arg) => {
            let url = arg.trim();
            if url.is_empty() {
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    "Usage: /debug <post url>",
                )
                .await?;
                return Ok(());
            }
            // Debug tool: report the parse result only — nothing is sent,
            // cached or forwarded.
            log::info!("debug: parsing [key={}]", log_key(url));
            match x_media::site::fetch(url).await {
                Ok(None) => {
                    reply(
                        bot,
                        message.chat.id.0,
                        message.id,
                        "No enabled site matches this link (twitter/x, pixiv, bsky, misskey or bilibili).",
                    )
                    .await?;
                }
                Err(e) => {
                    reply(
                        bot,
                        message.chat.id.0,
                        message.id,
                        format!("Fetch failed: {e}"),
                    )
                    .await?;
                }
                Ok(Some(fetched)) => {
                    let report = debug_report(
                        url,
                        fetched.site_name(),
                        &fetched.source_url,
                        &fetched.title,
                        fetched.render_fields(),
                        fetched.sensitive,
                        &fetched.caption,
                        &fetched.media,
                    );
                    // HTML report: the caption renders inside a <blockquote>
                    // exactly as it will appear in the sent media message.
                    reply_html(bot, message.chat.id.0, message.id, report).await?;
                }
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

/// Telegram's plain-text message limit is 4096 chars; the report stays under
/// it even for very large threads (many media lines + a long caption).
const MAX_DEBUG_REPORT_CHARS: usize = 4000;

/// Cap for the `/bot_dict` debug dump: the state is echoed as one plain-text
/// message, so it must stay under Telegram's 4096-char limit.
const MAX_DEBUG_DUMP_CHARS: usize = 3500;

/// Builds the HTML report for the `/debug` command: what the parser produced
/// for a link (site, canonical URL, title/author/tags, caption and the media
/// list) — no media is sent and nothing is cached or forwarded. Sent with
/// HTML parse mode: raw fields are escaped, the pre-escaped render fields are
/// embedded as-is, and the caption is wrapped in a `<blockquote>` so it shows
/// exactly as it will render in the sent media message. Fields are passed
/// individually so the formatter stays a pure function testable without
/// constructing a `Fetched` (its render fields are `pub(crate)` to the
/// x-media crate).
#[allow(clippy::too_many_arguments)]
fn debug_report(
    url: &str,
    site_id: &str,
    source_url: &str,
    title: &str,
    render: Option<(&str, &str, &str, &str)>,
    sensitive: bool,
    caption: &str,
    media: &[x_media::media::Media],
) -> String {
    let mut lines = vec![
        format!("Parse result for {}", html_escape::encode_text(url)),
        format!("site: {site_id}"),
        format!(
            "key: {}",
            html_escape::encode_text(
                &x_media::site::cache_key(url).unwrap_or_else(|| "<unsupported>".to_string())
            )
        ),
    ];
    lines.push(format!(
        "source_url: {}",
        html_escape::encode_text(source_url)
    ));
    lines.push(format!("title: {}", html_escape::encode_text(title)));
    if let Some((author, author_url, _title, tags)) = render {
        // The render fields are already pre-escaped for HTML captions; embed
        // them as-is so the report renders them exactly like the final
        // caption. `author_url` is raw and gets escaped here.
        lines.push(format!("author: {author}"));
        lines.push(format!(
            "author_url: {}",
            html_escape::encode_text(author_url)
        ));
        lines.push(format!("tags: {tags}"));
    }
    lines.push(format!("sensitive: {sensitive}"));
    // The caption is wrapped in a <blockquote> so the report (an HTML
    // message) shows it exactly as it will render in the sent media caption
    // — escaped text and links included.
    lines.push(format!(
        "caption: <blockquote>{}</blockquote>",
        x_media::site::truncate_caption(caption)
    ));
    lines.push(format!("media ({}):", media.len()));
    for (i, item) in media.iter().enumerate() {
        let kind = match item {
            x_media::media::Media::Illustration { .. } => "image",
            x_media::media::Media::Video { .. } => "video",
            x_media::media::Media::Animated { .. } => "gif",
        };
        lines.push(format!(
            "  {}. {kind}: {}",
            i + 1,
            html_escape::encode_text(item.url())
        ));
    }
    let mut out = lines.join(
        "
",
    );
    if out.chars().count() > MAX_DEBUG_REPORT_CHARS {
        let end = out.floor_char_boundary(MAX_DEBUG_REPORT_CHARS - 1);
        out = format!("{}…", &out[..end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEBUG_REPORT_CHARS, debug_report};
    use x_media::media::Media;

    #[test]
    fn debug_report_renders_fields_and_media() {
        let media = vec![
            Media::Illustration {
                title: None,
                url: "https://cdn.example/1.jpg".into(),
                thumbnail_url: None,
                fallback_url: None,
            },
            Media::Video {
                title: None,
                url: "https://cdn.example/2.mp4".into(),
                thumbnail_url: "https://cdn.example/2.jpg".into(),
            },
        ];
        let report = debug_report(
            "https://x.com/u/status/1",
            "twitter",
            "https://x.com/u/status/1",
            "My title",
            Some(("Author", "https://x.com/u", "My title", "tag1 tag2")),
            false,
            "<a href=\"https://x.com/u\">Author</a> · My title",
            &media,
        );
        assert!(report.contains("site: twitter"), "{report}");
        assert!(report.contains("key: twitter:1"), "{report}");
        assert!(report.contains("title: My title"), "{report}");
        assert!(report.contains("author: Author"), "{report}");
        assert!(report.contains("author_url: https://x.com/u"), "{report}");
        assert!(report.contains("tags: tag1 tag2"), "{report}");
        assert!(report.contains("sensitive: false"), "{report}");
        assert!(report.contains("media (2):"), "{report}");
        assert!(
            report.contains("1. image: https://cdn.example/1.jpg"),
            "{report}"
        );
        assert!(
            report.contains("2. video: https://cdn.example/2.mp4"),
            "{report}"
        );
    }

    #[test]
    fn debug_report_without_render_data_and_no_media() {
        let report = debug_report("u", "pixiv", "s", "t", None, true, "c", &[]);
        assert!(!report.contains("author:"), "{report}");
        assert!(report.contains("sensitive: true"), "{report}");
        assert!(report.contains("media (0):"), "{report}");
    }

    #[test]
    fn debug_report_wraps_caption_in_blockquote() {
        // The report is an HTML message: raw fields are escaped, pre-escaped
        // render fields are embedded as-is, and the caption is wrapped in a
        // <blockquote> so it shows exactly as it will render in the sent
        // media caption (escaped text and links included).
        let report = debug_report(
            "https://x.com/u/status/1",
            "twitter",
            "https://x.com/u/status/1",
            "A & B <C>",
            Some((
                "A &amp; B",
                "https://x.com/u",
                "A &amp; B &lt;C&gt;",
                "#a &amp; #b",
            )),
            false,
            "<a href=\"https://x.com/u\">A &amp; B</a>: C &lt;D&gt; &amp; E",
            &[],
        );
        // Raw fields escaped (they render back to the original text in HTML).
        assert!(report.contains("title: A &amp; B &lt;C&gt;"), "{report}");
        assert!(
            report.contains("source_url: https://x.com/u/status/1"),
            "{report}"
        );
        // Pre-escaped render fields embedded as-is.
        assert!(report.contains("author: A &amp; B"), "{report}");
        assert!(report.contains("tags: #a &amp; #b"), "{report}");
        // Caption wrapped in a blockquote with its HTML preserved.
        assert!(
            report.contains(
                "caption: <blockquote><a href=\"https://x.com/u\">A &amp; B</a>: C &lt;D&gt; &amp; E</blockquote>"
            ),
            "{report}"
        );
    }

    #[test]
    fn debug_report_is_capped() {
        // 200 media lines ≈ 8 KB, comfortably over the cap.
        let media: Vec<Media> = (0..200)
            .map(|i| Media::Illustration {
                title: None,
                url: format!("https://cdn.example/{i}.jpg"),
                thumbnail_url: None,
                fallback_url: None,
            })
            .collect();
        let report = debug_report("u", "twitter", "s", "t", None, false, "c", &media);
        assert!(report.chars().count() <= MAX_DEBUG_REPORT_CHARS, "{report}");
        assert!(report.ends_with('…'), "{report}");
    }
}
