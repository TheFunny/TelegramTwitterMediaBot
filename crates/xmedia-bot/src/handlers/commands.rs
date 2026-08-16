//! Bot command parsing, the `/`-command executor and `setMyCommands`
//! registration. URL/inline/callback flows live in their own modules.

use super::{CHAT_STORE, CONFIG, LINK_CACHE, log_key, reply};
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
    #[command(description = "Show chat state (debug)")]
    BotDict,
    #[command(description = "Set site caption format", parse_with = "split")]
    SetFormat(String),
    #[command(
        description = "Clear link cache (admin; optional URL, else all)",
        parse_with = "split"
    )]
    ClearCache(String),
    #[command(
        description = "Test link parsing (debug; no media sent)",
        parse_with = parse_test_arg
    )]
    Test(String),
}

/// `/test` argument parser: the whole remainder after the command name,
/// trimmed. The built-in `split` parser takes exactly one space-separated
/// token and rejects the rest, so a URL followed by a trailing space (or
/// pasted text) would silently fall through to the URL flow instead.
fn parse_test_arg(s: String) -> Result<(String,), ParseError> {
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
            let chat_data = CHAT_STORE.get(message.chat.id.0).await;
            let debug = format!("{chat_data:?}");
            let text = html_escape::encode_text(&debug).into_owned();
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
                            "Unrecognized link. Use a twitter/x, pixiv or bsky post URL.",
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
            // Debug tool: report the parse result only — nothing is sent,
            // cached or forwarded. Info level echoes the normalized key
            // (never the raw URL) per the logging convention.
            log::info!("test: parsing [key={}]", log_key(url));
            match x_media::site::fetch(url).await {
                Ok(None) => {
                    reply(
                        bot,
                        message.chat.id.0,
                        message.id,
                        "No enabled site matches this link (twitter/x, pixiv or bsky).",
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
                    let report = test_parse_report(
                        url,
                        fetched.site_name(),
                        &fetched.source_url,
                        &fetched.title,
                        fetched.render_fields(),
                        fetched.sensitive,
                        &fetched.caption,
                        &fetched.media,
                    );
                    reply(bot, message.chat.id.0, message.id, report).await?;
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
const MAX_TEST_REPORT_CHARS: usize = 4000;

/// Builds the plain-text report for the `/test` command: what the parser
/// produced for a link (site, canonical URL, title/author/tags, caption and
/// the media list) — no media is sent and nothing is cached or forwarded.
/// Fields are passed individually so the formatter stays a pure function
/// testable without constructing a `Fetched` (its render fields are
/// `pub(crate)` to the x-media crate).
#[allow(clippy::too_many_arguments)]
fn test_parse_report(
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
        format!("Parse result for {url}"),
        format!("site: {site_id}"),
        format!(
            "key: {}",
            x_media::site::cache_key(url).unwrap_or_else(|| "<unsupported>".to_string())
        ),
    ];
    lines.push(format!("source_url: {source_url}"));
    lines.push(format!("title: {title}"));
    if let Some((author, author_url, _title, tags)) = render {
        // The render fields are pre-escaped for HTML captions; decode them
        // so the plain-text report shows the text as it will be rendered
        // (no visible &amp; / &lt; / &gt;).
        lines.push(format!(
            "author: {}",
            html_escape::decode_html_entities(author)
        ));
        lines.push(format!("author_url: {author_url}"));
        lines.push(format!("tags: {}", html_escape::decode_html_entities(tags)));
    }
    lines.push(format!("sensitive: {sensitive}"));
    lines.push(format!(
        "caption: {}",
        x_media::site::truncate_caption(&html_escape::decode_html_entities(&strip_html_tags(
            caption
        )))
    ));
    lines.push(format!("media ({}):", media.len()));
    for (i, item) in media.iter().enumerate() {
        let kind = match item {
            x_media::media::Media::Illustration { .. } => "image",
            x_media::media::Media::Video { .. } => "video",
            x_media::media::Media::Animated { .. } => "gif",
        };
        lines.push(format!("  {}. {kind}: {}", i + 1, item.url()));
    }
    let mut out = lines.join(
        "
",
    );
    if out.chars().count() > MAX_TEST_REPORT_CHARS {
        let end = out.floor_char_boundary(MAX_TEST_REPORT_CHARS - 1);
        out = format!("{}…", &out[..end]);
    }
    out
}

/// Drops HTML tags from a caption for the plain-text `/test` report, keeping
/// the visible text (the links are reported separately via `source_url` /
/// `author_url`). Runs on the *escaped* caption: entity-encoded content
/// (`&lt;` `&amp;`) is not a tag and survives, then
/// [`html_escape::decode_html_entities`] renders the remaining text — so a
/// tweet text like `>^ω^<` stays intact instead of being eaten as markup.
/// Built-in captions are the only source of tags (custom formats are fully
/// escaped and contain none).
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_TEST_REPORT_CHARS, strip_html_tags, test_parse_report};
    use x_media::media::Media;

    #[test]
    fn test_parse_report_renders_fields_and_media() {
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
        let report = test_parse_report(
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
    fn test_parse_report_without_render_data_and_no_media() {
        let report = test_parse_report("u", "pixiv", "s", "t", None, true, "c", &[]);
        assert!(!report.contains("author:"), "{report}");
        assert!(report.contains("sensitive: true"), "{report}");
        assert!(report.contains("media (0):"), "{report}");
    }

    #[test]
    fn test_parse_report_renders_caption_as_plain_text() {
        // The report is a plain-text message: pre-escaped caption fields and
        // the HTML caption must be shown as rendered — tags stripped, entities
        // decoded — never with visible `<a href>` markup or &amp; / &lt; / &gt;.
        let report = test_parse_report(
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
        assert!(report.contains("title: A & B <C>"), "{report}");
        assert!(report.contains("author: A & B"), "{report}");
        assert!(report.contains("tags: #a & #b"), "{report}");
        // Anchor markup gone, entity-encoded text preserved through the strip
        // and then decoded.
        assert!(report.contains("caption: A & B: C <D> & E"), "{report}");
        for entity in ["&amp;", "&lt;", "&gt;"] {
            assert!(!report.contains(entity), "unexpected {entity} in: {report}");
        }
        assert!(!report.contains("<a href"), "raw markup in: {report}");
    }

    #[test]
    fn strip_html_tags_keeps_entity_encoded_text() {
        // The strip runs on the escaped caption: `&lt;` is an entity, not a
        // tag, and must survive so the subsequent decode renders it as `<`.
        assert_eq!(
            strip_html_tags("<a href=\"https://x.com/u\">A &amp; B</a>: &gt;^ω^&lt;"),
            "A &amp; B: &gt;^ω^&lt;"
        );
        assert_eq!(strip_html_tags("plain text"), "plain text");
    }

    #[test]
    fn test_parse_report_is_capped() {
        // 200 media lines ≈ 8 KB, comfortably over the cap.
        let media: Vec<Media> = (0..200)
            .map(|i| Media::Illustration {
                title: None,
                url: format!("https://cdn.example/{i}.jpg"),
                thumbnail_url: None,
                fallback_url: None,
            })
            .collect();
        let report = test_parse_report("u", "twitter", "s", "t", None, false, "c", &media);
        assert!(report.chars().count() <= MAX_TEST_REPORT_CHARS, "{report}");
        assert!(report.ends_with('…'), "{report}");
    }
}
