//! Bot command parsing, the `/`-command executor and `setMyCommands`
//! registration. URL/inline/callback flows live in their own modules.

use super::urls::{PostSend, url_media};
use super::{CHAT_STORE, CONFIG, LINK_CACHE, log_key, reply, reply_html};
use crate::ctx::AppContext;
use crate::state::ChatData;
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
    #[command(description = "Remove a saved template", parse_with = "split")]
    RemoveTemplate(String),
    #[command(description = "Show this chat's settings")]
    Settings,
    #[command(description = "Show chat state (debug; admin only)")]
    BotDict,
    #[command(
        description = "Set site caption format (- to reset)",
        parse_with = parse_arg_remainder
    )]
    SetFormat(String),
    #[command(
        description = "Clear link cache (admin; optional URL, else all)",
        parse_with = parse_arg_remainder
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

/// Placeholders `/set_format` accepts, mirroring what
/// `x_media::site::caption_from_fields` substitutes.
const FORMAT_PLACEHOLDERS: [&str; 6] = ["url", "author", "author_url", "title", "content", "tags"];

/// `/start`'s welcome: what the bot is for, where links work, where to look
/// next. The old "Hello!" left a first-time user with nothing.
const START_TEXT: &str = "\
Send me a post link and I'll send back its images, videos and GIFs with the title, author and tags.

Supported: X/Twitter, Pixiv, Bluesky, Misskey (misskey.io), Bilibili.
In a private chat just paste the link. In a group, use inline mode (type @, pick me, then the link).

/help lists every command.";

/// Appended to `/help`'s command list: argument syntax, caption
/// placeholders and the private-chat rule — none of which teloxide's
/// `descriptions()` renders (it prints `/command — description` only).
const HELP_FOOTER: &str = "\
Arguments
  /set_forward_channel <@channel or channel id>
  /set_template <name> — reply to a message containing [] to save it
  /remove_template <name> — see /settings for the saved names
  /set_format <site> <format> — '-' restores the built-in format
  /test <link> / /debug <link>

Caption placeholders (for /set_format)
  {url} {author} {author_url} {title} {content} {tags}
  A template's [] is replaced by the post link when forwarding.

Links are handled in private chats only; in a group use inline mode.";

/// Cap on template names echoed by `/settings`: a chat with hundreds of
/// templates must not produce a message Telegram rejects for length.
const MAX_SETTINGS_TEMPLATE_NAMES: usize = 30;

/// Sorted template names: the order `/settings`, `/remove_template` and the
/// prompt's buttons all show.
fn sorted_template_names(data: &ChatData) -> Vec<String> {
    let mut names: Vec<String> = data.template.keys().cloned().collect();
    names.sort();
    names
}

/// `/settings`: what this chat is configured to do, readable by anyone in it
/// (unlike `/bot_dict`, which dumps the raw state and is admin-only).
fn settings_text(data: &ChatData) -> String {
    let mut lines = Vec::new();
    match data.forward_channel_id {
        Some(id) => lines.push(format!("Forward channel: {id}")),
        None => lines.push(
            "Forward channel: not set (use /set_forward_channel <@channel or id>)".to_string(),
        ),
    }
    lines.push(format!(
        "Edit before forward: {}",
        if data.edit_before_forward {
            "on"
        } else {
            "off"
        }
    ));
    let mut formats: Vec<String> = data
        .message_format
        .iter()
        .map(|(site, format)| format!("{site} => {format}"))
        .collect();
    formats.sort();
    lines.push(if formats.is_empty() {
        "Caption formats: built-in for every site".to_string()
    } else {
        format!("Caption formats:\n  {}", formats.join("\n  "))
    });
    let names = sorted_template_names(data);
    lines.push(match names.len() {
        0 => "Templates: none".to_string(),
        n => format!(
            "Templates ({n}): {}{}",
            names
                .iter()
                .take(MAX_SETTINGS_TEMPLATE_NAMES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if n > MAX_SETTINGS_TEMPLATE_NAMES {
                format!(", +{} more", n - MAX_SETTINGS_TEMPLATE_NAMES)
            } else {
                String::new()
            }
        ),
    });
    lines.join("\n")
}

/// The first `{…}` token in a caption format that is not a known placeholder
/// (`None` when all of them are). The renderer replaces exact keys only, so an
/// unknown token would be published verbatim in every caption of that site —
/// caught here instead.
fn unknown_placeholder(format: &str) -> Option<&str> {
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        // An unclosed `{` is not a placeholder token at all.
        let close = after.find('}')?;
        let token = &after[..close];
        if !FORMAT_PLACEHOLDERS.contains(&token) {
            return Some(token);
        }
        rest = &after[close + 1..];
    }
    None
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
            bot.send_message(message.chat.id, START_TEXT).await?;
        }
        Command::Help => {
            // The command list plus the parts teloxide's `descriptions()`
            // cannot show: argument syntax, caption placeholders, and where a
            // link actually works.
            bot.send_message(
                message.chat.id,
                format!("{}\n\n{}", Command::descriptions(), HELP_FOOTER),
            )
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
        Command::RemoveTemplate(name) => {
            let chat_id = message.chat.id.0;
            let name = name.trim().to_string();
            if name.is_empty() {
                reply(
                    bot,
                    chat_id,
                    message.id,
                    "Usage: /remove_template <name> (see /settings for the saved names)",
                )
                .await?;
                return Ok(());
            }
            let removed = CHAT_STORE
                .update(chat_id, |data| data.template.remove(&name).is_some())
                .await;
            let text = if removed {
                format!("Template '{name}' removed.")
            } else {
                // Name the live templates: a typo would otherwise look like a
                // successful delete.
                let names = sorted_template_names(&CHAT_STORE.get(chat_id).await);
                if names.is_empty() {
                    format!("No template named '{name}'. None are saved yet.")
                } else {
                    format!("No template named '{name}'. Saved: {}", names.join(", "))
                }
            };
            reply(bot, chat_id, message.id, text).await?;
        }
        Command::Settings => {
            let chat_id = message.chat.id.0;
            let data = CHAT_STORE.get(chat_id).await;
            reply(bot, chat_id, message.id, settings_text(&data)).await?;
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
            // `-` resets to the site's built-in caption: without it a chat that
            // set a format once could never get back to the default (the
            // built-in format string is not something a user can retype).
            if format == "-" {
                CHAT_STORE
                    .update(chat_id, |data| {
                        data.message_format.remove(site);
                    })
                    .await;
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    "Format reset to the built-in one.",
                )
                .await?;
                return Ok(());
            }
            // A typo like {titel} would otherwise be rendered literally into
            // every caption of that site (the renderer only substitutes the
            // exact keys), which is invisible until a post arrives.
            if let Some(token) = unknown_placeholder(&format) {
                reply(
                    bot,
                    message.chat.id.0,
                    message.id,
                    format!(
                        "Unknown placeholder {{{token}}}. Available: {}",
                        FORMAT_PLACEHOLDERS
                            .iter()
                            .map(|name| format!("{{{name}}}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                )
                .await?;
                return Ok(());
            }
            CHAT_STORE
                .update(chat_id, |data| {
                    data.message_format.insert(site.to_string(), format);
                })
                .await;
            reply(
                bot,
                message.chat.id.0,
                message.id,
                "Format set. Use /debug <link> to preview the caption.",
            )
            .await?;
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
                    // The preview must show what a link would actually send:
                    // the chat's per-site format override plus the long-post
                    // quoting. Rendering the raw built-in caption here made
                    // `/set_format` look like it did nothing.
                    let format = CHAT_STORE
                        .get(message.chat.id.0)
                        .await
                        .format_for(fetched.site_id);
                    let caption = preview_caption(
                        &format,
                        &fetched.caption,
                        &fetched.source_url,
                        fetched.render_fields(),
                        CONFIG.caption_quote_text_chars,
                    );
                    let report = debug_report(
                        url,
                        fetched.site_id,
                        &fetched.source_url,
                        &fetched.title,
                        &fetched.content,
                        fetched.render_fields(),
                        fetched.sensitive,
                        &caption,
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

/// Bot profile texts (Bot API `setMyDescription` / `setMyShortDescription`):
/// shown on the bot's profile page and in the share sheet. Without them a
/// shared link says nothing about what the bot does.
const BOT_DESCRIPTION: &str = "\
Send a post link from X/Twitter, Pixiv, Bluesky, Misskey (misskey.io) or Bilibili and get its images, videos and GIFs back with the title, author and tags.
Links are handled in private chats; a group can use inline mode. /help lists every command.";
const BOT_SHORT_DESCRIPTION: &str =
    "Post links (X, Pixiv, Bluesky, Misskey, Bilibili) -> media messages";

/// Registers the bot's command list with Telegram so clients show it in the
/// `/` menu (Bot API `setMyCommands`), plus its profile description texts.
pub async fn register_commands(bot: &Bot) -> Result<(), RequestError> {
    let commands = Command::bot_commands();
    bot.set_my_commands(commands.clone()).await?;
    log::info!("registered {} commands", commands.len());
    // Profile texts are cosmetic: a failure (rare) must not abort startup.
    if let Err(e) = bot.set_my_description().description(BOT_DESCRIPTION).await {
        log::warn!("failed to set the bot description: {e}");
    }
    if let Err(e) = bot
        .set_my_short_description()
        .short_description(BOT_SHORT_DESCRIPTION)
        .await
    {
        log::warn!("failed to set the bot short description: {e}");
    }
    Ok(())
}

/// Telegram's plain-text message limit is 4096 chars; the report stays under
/// it even for very large threads (many media lines + a long caption).
const MAX_DEBUG_REPORT_CHARS: usize = 4000;

/// Cap for the `/bot_dict` debug dump: the state is echoed as one plain-text
/// message, so it must stay under Telegram's 4096-char limit.
const MAX_DEBUG_DUMP_CHARS: usize = 3500;

/// The caption a link would actually send for this chat: the per-site format
/// override (empty = the site's built-in caption) and, on a long post, the
/// same text quoting the send paths apply. `/debug` shows this so the preview
/// cannot drift from what the send paths produce.
fn preview_caption(
    format: &str,
    built_in: &str,
    url: &str,
    fields: Option<(&str, &str, &str, &str, &str)>,
    quote_chars: usize,
) -> String {
    let caption = match fields {
        // Same call the send paths make through `Fetched::caption_with`: an
        // empty format falls back to the built-in caption.
        Some((author, author_url, title, content, tags)) => x_media::site::caption_from_fields(
            format, built_in, url, author, author_url, title, content, tags,
        ),
        None => x_media::site::truncate_caption(built_in),
    };
    let text = fields
        .map(|(_, _, title, content, _)| x_media::site::compose_text(title, content))
        .unwrap_or_default();
    crate::send::quote_long_caption(&caption, &text, quote_chars).into_owned()
}

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
    content: &str,
    render: Option<(&str, &str, &str, &str, &str)>,
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
    lines.push(format!("content: {}", html_escape::encode_text(content)));
    if let Some((author, author_url, _title, _content, tags)) = render {
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
    use super::{
        MAX_DEBUG_REPORT_CHARS, debug_report, preview_caption, settings_text, unknown_placeholder,
    };
    use x_media::media::Media;

    #[test]
    fn debug_report_renders_fields_and_media() {
        let media = vec![
            Media::Illustration {
                url: "https://cdn.example/1.jpg".into(),
                thumbnail_url: None,
                fallback_url: None,
            },
            Media::Video {
                url: "https://cdn.example/2.mp4".into(),
                thumbnail_url: "https://cdn.example/2.jpg".into(),
            },
        ];
        let report = debug_report(
            "https://x.com/u/status/1",
            "twitter",
            "https://x.com/u/status/1",
            "My title",
            "My content",
            Some((
                "Author",
                "https://x.com/u",
                "My title",
                "My content",
                "tag1 tag2",
            )),
            false,
            "<a href=\"https://x.com/u\">Author</a> · My title",
            &media,
        );
        assert!(report.contains("site: twitter"), "{report}");
        assert!(report.contains("key: twitter:1"), "{report}");
        assert!(report.contains("content: My content"), "{report}");
        assert!(report.contains("author_url: https://x.com/u"), "{report}");
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
    fn debug_report_without_render_data_has_no_author_line() {
        let report = debug_report("u", "pixiv", "s", "t", "c", None, true, "p", &[]);
        // The `None` branch above is the point: with no render fields there is
        // no author line to print. The `sensitive`/`media` lines are the same
        // format sites the escaping test already pins with values.
        assert!(!report.contains("author:"), "{report}");
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
            "body & <more>",
            Some((
                "A &amp; B",
                "https://x.com/u",
                "A &amp; B &lt;C&gt;",
                "body &amp; &lt;more&gt;",
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
                url: format!("https://cdn.example/{i}.jpg"),
                thumbnail_url: None,
                fallback_url: None,
            })
            .collect();
        let report = debug_report("u", "twitter", "s", "t", "c", None, false, "p", &media);
        assert!(report.chars().count() <= MAX_DEBUG_REPORT_CHARS, "{report}");
        assert!(report.ends_with('…'), "{report}");
    }

    #[test]
    fn settings_text_reports_the_chat_configuration() {
        use crate::state::ChatData;

        // A fresh chat: the defaults must be spelled out, including how to set
        // the channel (an empty field is not a status).
        let empty = settings_text(&ChatData::default());
        assert!(empty.contains("Forward channel: not set"), "{empty}");
        assert!(empty.contains("/set_forward_channel"), "{empty}");
        assert!(empty.contains("Edit before forward: off"), "{empty}");
        assert!(empty.contains("built-in for every site"), "{empty}");
        assert!(empty.contains("Templates: none"), "{empty}");

        let configured = ChatData {
            forward_channel_id: Some(-100123),
            edit_before_forward: true,
            template: [("b", "[]"), ("a", "[]")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            message_format: [("twitter", "{author}: {content}")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..ChatData::default()
        };
        let text = settings_text(&configured);
        assert!(text.contains("Forward channel: -100123"), "{text}");
        assert!(text.contains("Edit before forward: on"), "{text}");
        assert!(text.contains("twitter => {author}: {content}"), "{text}");
        // Sorted, so the same chat always reports the same thing.
        assert!(text.contains("Templates (2): a, b"), "{text}");
    }

    #[test]
    fn help_and_start_cover_what_the_command_list_cannot() {
        // The placeholders the renderer substitutes must be the ones the help
        // lists: a stale list is worse than none.
        for placeholder in super::FORMAT_PLACEHOLDERS {
            assert!(
                super::HELP_FOOTER.contains(&format!("{{{placeholder}}}")),
                "help does not document {{{placeholder}}}"
            );
        }
        // The private-chat rule and the template placeholder semantics are the
        // two things users got wrong most often.
        assert!(super::HELP_FOOTER.contains("private chats only"));
        assert!(super::HELP_FOOTER.contains("[]"));
        assert!(super::START_TEXT.contains("inline mode"));
        assert!(super::START_TEXT.contains("/help"));
        // Both must stay inside Telegram's message limit.
        assert!(super::HELP_FOOTER.chars().count() < 2000);
        assert!(super::START_TEXT.chars().count() < 2000);
    }

    #[test]
    fn every_command_is_registered_and_parses() {
        use teloxide::utils::command::BotCommands;

        use super::Command;

        let registered: Vec<String> = Command::bot_commands()
            .into_iter()
            .map(|command| command.command.trim_start_matches('/').to_string())
            .collect();
        for expected in [
            "start",
            "help",
            "settings",
            "set_forward_channel",
            "remove_template",
            "set_format",
            "test",
            "debug",
        ] {
            assert!(
                registered.iter().any(|name| name == expected),
                "{expected} missing from {registered:?}"
            );
        }
        // Telegram caps a command description at 256 chars.
        for command in Command::bot_commands() {
            assert!(
                command.description.chars().count() <= 256,
                "{}: description too long",
                command.command
            );
        }
    }

    #[test]
    fn every_documented_invocation_parses() {
        use teloxide::utils::command::BotCommands;

        use super::Command;

        // The README's forms, verbatim. teloxide's `split` parser accepts
        // EXACTLY one token per `String` field, so a command documented with
        // two arguments (or an optional one) silently stops parsing — and a
        // command that does not parse falls through to the URL flow in
        // silence.
        type Check = fn(&Command) -> bool;
        let cases: Vec<(&str, Check)> = vec![
            ("/start", |c| matches!(c, Command::Start)),
            ("/help", |c| matches!(c, Command::Help)),
            ("/settings", |c| matches!(c, Command::Settings)),
            ("/edit_before_forward", |c| {
                matches!(c, Command::EditBeforeForward)
            }),
            ("/remove_forward_channel", |c| {
                matches!(c, Command::RemoveForwardChannel)
            }),
            ("/bot_dict", |c| matches!(c, Command::BotDict)),
            (
                "/set_forward_channel @a_channel",
                |c| matches!(c, Command::SetForwardChannel(a) if a == "@a_channel"),
            ),
            (
                "/set_template tpl",
                |c| matches!(c, Command::SetTemplate(a) if a == "tpl"),
            ),
            (
                "/remove_template tpl",
                |c| matches!(c, Command::RemoveTemplate(a) if a == "tpl"),
            ),
            (
                "/set_format twitter {author}: {title}",
                |c| matches!(c, Command::SetFormat(a) if a == "twitter {author}: {title}"),
            ),
            (
                "/set_format twitter -",
                |c| matches!(c, Command::SetFormat(a) if a == "twitter -"),
            ),
            // Documented as "clear everything" when called without a link.
            (
                "/clear_cache",
                |c| matches!(c, Command::ClearCache(a) if a.is_empty()),
            ),
            (
                "/clear_cache https://x.com/u/status/1",
                |c| matches!(c, Command::ClearCache(a) if a == "https://x.com/u/status/1"),
            ),
            (
                "/test https://x.com/u/status/1",
                |c| matches!(c, Command::Test(a) if a == "https://x.com/u/status/1"),
            ),
            (
                "/debug https://x.com/u/status/1",
                |c| matches!(c, Command::Debug(a) if a == "https://x.com/u/status/1"),
            ),
        ];

        for (text, ok) in cases {
            match Command::parse(text, "") {
                Ok(parsed) => assert!(ok(&parsed), "{text} parsed as the wrong variant"),
                Err(e) => panic!("{text} did not parse: {e}"),
            }
        }
    }

    #[test]
    fn preview_caption_applies_the_chat_format_and_the_long_post_quote() {
        let fields = Some((
            "Author",
            "https://x.com/u",
            "Pinned title",
            "Pinned body",
            "#tag",
        ));

        // No format override → the site's built-in caption, untouched.
        assert_eq!(
            preview_caption(
                "",
                "built-in caption",
                "https://x.com/u/status/1",
                fields,
                200
            ),
            "built-in caption"
        );

        // The bug this pins: `/debug` used to print the built-in caption even
        // with a format set, so `/set_format` looked like it did nothing.
        let formatted = preview_caption(
            "{author} · {title}",
            "built-in caption",
            "https://x.com/u/status/1",
            fields,
            200,
        );
        assert_eq!(formatted, "Author · Pinned title");

        // `{url}` comes from the canonical post URL, as in the send paths.
        assert_eq!(
            preview_caption(
                "{url} {title}",
                "built-in",
                "https://x.com/u/status/1",
                fields,
                200
            ),
            "https://x.com/u/status/1 Pinned title"
        );

        // A long post's text is quoted exactly like the send paths quote it.
        let long = "正".repeat(300);
        let fields = Some(("Author", "https://x.com/u", "", long.as_str(), ""));
        let quoted = preview_caption(
            "",
            "https://x.com/u/status/1\n<a href=\"https://x.com/u\">Author</a>: 正…",
            "https://x.com/u/status/1",
            fields,
            200,
        );
        assert!(quoted.contains("<blockquote expandable>"), "{quoted}");

        // Without render fields (a site that does not expose them) the
        // built-in caption is all there is.
        assert_eq!(
            preview_caption("", "built-in", "https://x.com/u/status/1", None, 200),
            "built-in"
        );
    }

    #[test]
    fn unknown_placeholder_finds_typos_only() {
        assert_eq!(unknown_placeholder("{author} — {title}"), None);
        // Every key the renderer substitutes must pass, in any combination.
        assert_eq!(
            unknown_placeholder("{url}{author}{author_url}{title}{content}{tags}"),
            None
        );
        // Plain text and braces Telegram renders literally are not tokens.
        assert_eq!(unknown_placeholder("no placeholders here"), None);
        assert_eq!(unknown_placeholder("{unclosed"), None);

        assert_eq!(unknown_placeholder("{titel}"), Some("titel"));
        assert_eq!(unknown_placeholder("{title} {Content}"), Some("Content"));
        // A typo after a valid token is still found.
        assert_eq!(unknown_placeholder("{url} {tag}"), Some("tag"));
    }
}
