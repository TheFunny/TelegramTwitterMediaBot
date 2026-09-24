//! Bot command parsing, the `/`-command executor and `setMyCommands`
//! registration. URL/inline/callback flows live in their own modules.

use super::urls::{PostSend, url_media};
use super::{log_key, reply};
use crate::ctx::AppContext;
use crate::state::ChatData;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Message, ParseMode, Recipient, ReplyParameters};
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

const CHAT_STATE_READ_ERROR: &str = "Couldn't read chat settings; try again.";

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
/// Telegram's callback data limit is 64 bytes. Reserve the `template|` prefix
/// so a name can always be carried by a prompt button.
const MAX_TEMPLATE_NAME_BYTES: usize = 64 - "template|".len();
/// Keep the persisted map bounded well below the prompt keyboard's 60-button
/// cap so every stored template remains usable in a prompt.
const MAX_TEMPLATES: usize = 50;
/// Keep the persisted template body within a caption-sized value. Validation
/// applies to the escaped body after `html_escape::encode_text` (before the
/// `[]` placeholder is substituted at apply time).
const MAX_TEMPLATE_BODY_CHARS: usize = x_media::site::MAX_CAPTION_CHARS;
const MAX_SETTINGS_CHARS: usize = 4000;

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
    cap_text(lines.join("\n"), MAX_SETTINGS_CHARS)
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
            super::log_escape(&from.full_name()),
            message.chat.id,
            super::log_escape(&channel.to_string())
        );
    }
    let chat = match bot.get_chat(channel.clone()).await {
        Err(e) => {
            log::warn!(
                "Failed to get channel {}: {}",
                super::log_escape(&channel.to_string()),
                e
            );
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
    // chats), so the old check broke group usage).
    let Some(sender) = message.from.as_ref() else {
        return Err(SetForwardChannelError::NotAdmin);
    };
    match bot.get_chat_administrators(channel.clone()).await {
        Err(e) => {
            log::warn!(
                "Failed to get channel administrators {}: {}",
                super::log_escape(&channel.to_string()),
                e
            );
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

/// Runs one parsed command. Takes its context (stores + the sender) and the
/// `Bot`, the same shape [`crate::handlers::handle_message`] uses: `bot` is
/// only for the calls the [`MediaSender`] surface does not carry (channel
/// admin lookups, the HTML-parse-mode report).
///
/// [`MediaSender`]: crate::media_sender::MediaSender
pub(crate) async fn execute_command(
    ctx: &AppContext<'_>,
    bot: &Bot,
    message: &Message,
    command: Command,
) -> Result<(), RequestError> {
    match command {
        Command::Start => {
            ctx.sender
                .send_message(message.chat.id, START_TEXT.to_string(), None, None)
                .await?;
        }
        Command::Help => {
            // The command list plus the parts teloxide's `descriptions()`
            // cannot show: argument syntax, caption placeholders, and where a
            // link actually works.
            ctx.sender
                .send_message(
                    message.chat.id,
                    format!("{}\n\n{}", Command::descriptions(), HELP_FOOTER),
                    None,
                    None,
                )
                .await?;
        }
        Command::SetForwardChannel(channel) => {
            let result = match set_forward_channel_handler(bot, message, channel).await {
                Ok(channel_id) => match ctx
                    .chat_store
                    .update(message.chat.id.0, |data| {
                        data.forward_channel_id = Some(channel_id);
                    })
                    .await
                {
                    Ok((_, true)) => "Add successfully.".to_string(),
                    Ok((_, false)) => {
                        "Forward channel set only in memory; retry later.".to_string()
                    }
                    Err(()) => CHAT_STATE_READ_ERROR.to_string(),
                },
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
            reply(ctx.sender, message.chat.id.0, message.id, result).await?;
        }
        Command::RemoveForwardChannel => {
            let chat_id = message.chat.id.0;
            let text = match ctx
                .chat_store
                .update(chat_id, |data| {
                    if data.forward_channel_id.is_some() {
                        data.forward_channel_id = None;
                        "Remove successfully.".to_string()
                    } else {
                        "No channel to remove.".to_string()
                    }
                })
                .await
            {
                Ok((text, _)) => text,
                Err(()) => CHAT_STATE_READ_ERROR.to_string(),
            };
            reply(ctx.sender, message.chat.id.0, message.id, text).await?;
        }
        Command::EditBeforeForward => {
            let chat_id = message.chat.id.0;
            let text = match ctx
                .chat_store
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
                .await
            {
                Ok((text, _)) => text,
                Err(()) => CHAT_STATE_READ_ERROR.to_string(),
            };
            reply(ctx.sender, message.chat.id.0, message.id, text).await?;
        }
        Command::SetTemplate(name) => {
            let chat_id = message.chat.id.0;
            let name = name.trim().to_string();
            let text = match message.reply_to_message() {
                None => "Please reply to a message to set as template.".to_string(),
                Some(reply) => {
                    let reply_text = reply.text().unwrap_or_default();
                    if !reply_text.contains("[]") {
                        "Please reply to a message with [] to set as template.".to_string()
                    } else if name.is_empty() {
                        "Please provide a name for the template.".to_string()
                    } else if name.len() > MAX_TEMPLATE_NAME_BYTES {
                        format!("Template name is too long (max {MAX_TEMPLATE_NAME_BYTES} bytes).")
                    } else {
                        let template = html_escape::encode_text(reply_text);
                        if template.chars().count() > MAX_TEMPLATE_BODY_CHARS {
                            format!(
                                "Template is too long (max {MAX_TEMPLATE_BODY_CHARS} characters)."
                            )
                        } else {
                            match ctx
                                .chat_store
                                .update(chat_id, |data| {
                                    if data.template.len() >= MAX_TEMPLATES
                                        && !data.template.contains_key(&name)
                                    {
                                        return Err(());
                                    }
                                    data.template.insert(name.clone(), template.into_owned());
                                    Ok(())
                                })
                                .await
                            {
                                Ok((Ok(()), true)) => "Template set.".to_string(),
                                Ok((Ok(()), false)) => {
                                    "Template set only in memory; retry later.".to_string()
                                }
                                Ok((Err(()), _)) => format!(
                                    "This chat already has the maximum of {MAX_TEMPLATES} templates."
                                ),
                                Err(()) => CHAT_STATE_READ_ERROR.to_string(),
                            }
                        }
                    }
                }
            };
            reply(ctx.sender, message.chat.id.0, message.id, text).await?;
        }
        Command::RemoveTemplate(name) => {
            let chat_id = message.chat.id.0;
            let name = name.trim().to_string();
            if name.is_empty() {
                reply(
                    ctx.sender,
                    chat_id,
                    message.id,
                    "Usage: /remove_template <name> (see /settings for the saved names)",
                )
                .await?;
                return Ok(());
            }
            let text = match ctx
                .chat_store
                .update(chat_id, |data| data.template.remove(&name).is_some())
                .await
            {
                Ok((true, _)) => format!("Template '{name}' removed."),
                Ok((false, _)) => {
                    let names = sorted_template_names(&ctx.chat_store.get(chat_id).await);
                    if names.is_empty() {
                        format!("No template named '{name}'. None are saved yet.")
                    } else {
                        format!("No template named '{name}'. Saved: {}", names.join(", "))
                    }
                }
                Err(()) => CHAT_STATE_READ_ERROR.to_string(),
            };
            reply(ctx.sender, chat_id, message.id, text).await?;
        }
        Command::Settings => {
            let chat_id = message.chat.id.0;
            let data = ctx.chat_store.get(chat_id).await;
            reply(ctx.sender, chat_id, message.id, settings_text(&data)).await?;
        }
        Command::BotDict => {
            // Debug dump of the chat's persisted state: admin only (it echoes
            // forward-channel ids and templates to whoever asks).
            if require_admin(ctx, message).await?.is_none() {
                return Ok(());
            }
            let chat_data = ctx.chat_store.get(message.chat.id.0).await;
            let debug = html_escape::encode_text(&format!("{chat_data:?}")).into_owned();
            // A chat with many templates/edit records exceeds Telegram's 4096
            // char message limit; the dump is plain text (no parse mode), so a
            // plain byte-boundary cut is safe.
            let text = cap_text(debug, MAX_DEBUG_DUMP_CHARS);
            reply(ctx.sender, message.chat.id.0, message.id, text).await?;
        }
        Command::SetFormat(arg) => {
            let chat_id = message.chat.id.0;
            let (site, format) = match arg.split_once(char::is_whitespace) {
                Some((site, format)) if !format.trim().is_empty() => {
                    (site.trim(), format.trim().to_string())
                }
                _ => {
                    reply(
                        ctx.sender,
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
                    ctx.sender,
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
                let text = match ctx
                    .chat_store
                    .update(chat_id, |data| {
                        data.message_format.remove(site);
                    })
                    .await
                {
                    Ok((_, true)) => "Format reset to the built-in one.".to_string(),
                    Ok((_, false)) => "Reset in memory only; retry later.".to_string(),
                    Err(()) => CHAT_STATE_READ_ERROR.to_string(),
                };
                reply(ctx.sender, message.chat.id.0, message.id, text).await?;
                return Ok(());
            }
            // A typo like {titel} would otherwise be rendered literally into
            // every caption of that site (the renderer only substitutes the
            // exact keys), which is invisible until a post arrives.
            if let Some(token) = unknown_placeholder(&format) {
                reply(
                    ctx.sender,
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
            let text = match ctx
                .chat_store
                .update(chat_id, |data| {
                    data.message_format.insert(site.to_string(), format);
                })
                .await
            {
                Ok((_, true)) => {
                    "Format set. Use /debug <link> to preview the caption.".to_string()
                }
                Ok((_, false)) => "Format set only in memory; retry later.".to_string(),
                Err(()) => CHAT_STATE_READ_ERROR.to_string(),
            };
            reply(ctx.sender, message.chat.id.0, message.id, text).await?;
        }
        Command::ClearCache(arg) => {
            let Some(sender_id) = require_admin(ctx, message).await? else {
                return Ok(());
            };
            let arg = arg.trim();
            if arg.is_empty() {
                let removed = ctx.link_cache.clear(None).await;
                log::info!("cache cleared by {sender_id}: {removed} entries");
                reply(
                    ctx.sender,
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
                            ctx.sender,
                            message.chat.id.0,
                            message.id,
                            "Unrecognized link. Use a twitter/x, pixiv, bsky, misskey or bilibili post URL.",
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let removed = ctx.link_cache.clear(Some(&key)).await;
                log::info!("cache entry cleared by {sender_id}: {key} ({removed} rows)");
                reply(
                    ctx.sender,
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
                    ctx.sender,
                    message.chat.id.0,
                    message.id,
                    "Usage: /test <post url>",
                )
                .await?;
                return Ok(());
            }
            if x_media::site::cache_key(url).is_none() {
                reply(
                    ctx.sender,
                    message.chat.id.0,
                    message.id,
                    "No enabled site matches this link (twitter/x, pixiv, bsky, misskey or bilibili).",
                )
                .await?;
                return Ok(());
            }
            let _command_fetch = x_media::site::acquire_command_fetch_slot().await;
            log::info!("test: sending [key={}]", log_key(url));
            url_media(
                ctx,
                message.chat.id.0,
                message.id.0 as i64,
                url,
                PostSend::Suppressed,
            )
            .await;
            drop(_command_fetch);
        }
        Command::Debug(arg) => {
            let url = arg.trim();
            if url.is_empty() {
                reply(
                    ctx.sender,
                    message.chat.id.0,
                    message.id,
                    "Usage: /debug <post url>",
                )
                .await?;
                return Ok(());
            }
            let _command_fetch = x_media::site::acquire_command_fetch_slot().await;
            log::info!("debug: parsing [key={}]", log_key(url));
            match x_media::site::fetch(url).await {
                Ok(None) => {
                    reply(
                        ctx.sender,
                        message.chat.id.0,
                        message.id,
                        "No enabled site matches this link (twitter/x, pixiv, bsky, misskey or bilibili).",
                    )
                    .await?;
                }
                Err(e) => {
                    reply(
                        ctx.sender,
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
                    let format = ctx
                        .chat_store
                        .get(message.chat.id.0)
                        .await
                        .format_for(fetched.site_id);
                    let caption = preview_caption(
                        &format,
                        &fetched.caption,
                        &fetched.source_url,
                        fetched.render_fields(),
                        ctx.config.caption_quote_text_chars,
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
                    // `<Bot as Requester>::` disambiguates from the
                    // MediaSender trait's same-named method (see
                    // media_sender.rs).
                    <Bot as Requester>::send_message(bot, ChatId(message.chat.id.0), report)
                        .parse_mode(ParseMode::Html)
                        .reply_parameters(
                            ReplyParameters::new(message.id).allow_sending_without_reply(),
                        )
                        .await?;
                }
            }
        }
    }
    Ok(())
}

/// The gate the admin-only commands share: `Some(sender_id)` for an admin,
/// `None` after the refusal has been sent (the command then returns).
async fn require_admin(
    ctx: &AppContext<'_>,
    message: &Message,
) -> Result<Option<i64>, RequestError> {
    let sender_id = message
        .from
        .as_ref()
        .map(|user| user.id.0 as i64)
        .unwrap_or(-1);
    if ctx.config.admin_ids.contains(&sender_id) {
        return Ok(Some(sender_id));
    }
    reply(ctx.sender, message.chat.id.0, message.id, "Admin only.").await?;
    Ok(None)
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

/// Truncates `text` to at most `max` characters. The byte boundary keeps the
/// result valid UTF-8; Telegram's message limit is character-based, so this
/// remains conservative for non-ASCII text.
fn cap_text(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let end = text.floor_char_boundary(max.saturating_sub(1));
    format!("{}…", &text[..end])
}

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
    // — escaped text and links included. A long post's caption already
    // carries quote_long_caption's expandable blockquote and the API rejects
    // nested ones (the same rule quote_long_caption applies), so that caption
    // is shown unwrapped instead of failing to send.
    let caption = x_media::site::truncate_caption(caption);
    let caption = if caption.contains("<blockquote") {
        caption
    } else {
        format!("<blockquote>{caption}</blockquote>")
    };
    lines.push(format!("caption: {caption}"));
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
    let out = lines.join(
        "
",
    );
    cap_text(out, MAX_DEBUG_REPORT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::{
        Command, MAX_DEBUG_REPORT_CHARS, cap_text, debug_report, execute_command, preview_caption,
        settings_text, unknown_placeholder,
    };
    use crate::ctx::test_support::{TestStores, api_error, cached_photo};
    use crate::media_sender::test_support::{MockSender, Outcome};
    use std::time::Duration;
    use teloxide::Bot;
    use teloxide::types::Message;
    use x_media::media::Media;

    /// A private message from `user_id`, as the dispatcher would hand it over.
    fn message_from(user_id: i64, text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "message_id": 2,
            "date": 0,
            "chat": { "id": 1, "type": "private" },
            "from": { "id": user_id, "is_bot": false, "first_name": "u" },
            "text": text,
        }))
        .expect("a minimal message deserializes")
    }

    /// The executor's wiring, which had no test while it reached for the
    /// process-wide statics: each command reads and writes the chat's own
    /// store and answers through the sender it was given.
    #[tokio::test]
    async fn the_executor_uses_the_context_it_is_given() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let bot = Bot::new("42:TEST");
        let message = message_from(5, "/settings");

        execute_command(&ctx, &bot, &message, Command::Settings)
            .await
            .unwrap();
        assert!(
            sender.messages()[0].contains("Forward channel: not set"),
            "{:?}",
            sender.messages()
        );

        // `/set_format` writes the chat's override, the `-` form removes it
        // again; neither touches another chat.
        execute_command(
            &ctx,
            &bot,
            &message,
            Command::SetFormat("twitter {author}: {content}".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            stores.chat_store().get(1).await.format_for("twitter"),
            "{author}: {content}"
        );
        execute_command(&ctx, &bot, &message, Command::SetFormat("twitter -".into()))
            .await
            .unwrap();
        assert_eq!(stores.chat_store().get(1).await.format_for("twitter"), "");

        // A typo'd placeholder is refused (and not stored): it would otherwise
        // render literally into every caption of that site.
        execute_command(
            &ctx,
            &bot,
            &message,
            Command::SetFormat("twitter {titel}".into()),
        )
        .await
        .unwrap();
        let last = sender.messages().last().unwrap().clone();
        assert!(last.contains("Unknown placeholder {titel}"), "{last}");
        assert_eq!(stores.chat_store().get(1).await.format_for("twitter"), "");
    }

    /// The admin gate: the two admin-only commands answer a refusal instead of
    /// acting, and act for an admin.
    #[tokio::test]
    async fn the_admin_only_commands_refuse_a_non_admin() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk], || api_error("boom"));
        let mut stores = TestStores::new();
        stores.config_mut().admin_ids = vec![5];
        stores.link_cache().put("twitter:1", &cached_photo()).await;
        let ctx = stores.ctx(&sender);
        let bot = Bot::new("42:TEST");
        let outsider = message_from(9, "/clear_cache");

        for command in [
            Command::BotDict,
            Command::ClearCache(String::new()),
            Command::ClearCache("https://x.com/u/status/1".into()),
        ] {
            execute_command(&ctx, &bot, &outsider, command)
                .await
                .unwrap();
        }
        assert_eq!(
            sender.messages(),
            vec!["Admin only."; 3],
            "every admin-only command answers the refusal"
        );
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(60))
                .await
                .is_some(),
            "a refusal must not clear the cache"
        );

        // The admin's `/clear_cache` does clear it, by link and wholesale.
        let admin = message_from(5, "/clear_cache");
        execute_command(
            &ctx,
            &bot,
            &admin,
            Command::ClearCache("https://x.com/u/status/1".into()),
        )
        .await
        .unwrap();
        assert!(
            stores
                .link_cache()
                .get("twitter:1", Duration::from_secs(60))
                .await
                .is_none(),
            "the admin's /clear_cache must clear the entry"
        );
    }

    /// `/debug` answers the parse result and sends nothing: an unsupported link
    /// gets the explanation the group/private paths also use.
    #[tokio::test]
    async fn debug_replies_without_sending_media() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let bot = Bot::new("42:TEST");
        let message = message_from(5, "/debug https://example.com/x");

        execute_command(
            &ctx,
            &bot,
            &message,
            Command::Debug("https://example.com/x".into()),
        )
        .await
        .unwrap();

        assert_eq!(sender.calls(), vec!["send_message"]);
        assert!(
            sender.messages()[0].contains("No enabled site matches this link"),
            "{:?}",
            sender.messages()
        );
    }

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
    fn debug_report_does_not_nest_a_quoted_caption() {
        // A long post's preview_caption already carries quote_long_caption's
        // <blockquote expandable>; wrapping it again produced nested
        // blockquotes, which the API rejects — /debug on any long post 400'd.
        let quoted = "intro <blockquote expandable>long text</blockquote>";
        let report = debug_report(
            "https://x.com/u/status/1",
            "twitter",
            "https://x.com/u/status/1",
            "t",
            "c",
            None,
            false,
            quoted,
            &[],
        );
        assert!(
            report.contains(&format!("caption: {quoted}")),
            "the quoted caption must be shown as-is: {report}"
        );
        assert_eq!(
            report.matches("<blockquote").count(),
            1,
            "no outer wrapper may be added: {report}"
        );
    }

    #[test]
    fn cap_text_cuts_on_a_char_boundary() {
        assert_eq!(cap_text("short".into(), 10), "short");
        assert_eq!(cap_text("exactly".into(), 7), "exactly");
        assert_eq!(cap_text("truncated".into(), 5), "trun…");
        // A multi-byte character at the cut is dropped whole, not split.
        assert_eq!(cap_text("aaaa漢bb".into(), 6), "aaaa…");
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
    fn settings_text_is_capped_before_telegram_limit() {
        use crate::state::ChatData;

        let data = ChatData {
            message_format: [("twitter", "x".repeat(4000))]
                .into_iter()
                .map(|(site, format)| (site.to_string(), format))
                .collect(),
            template: (0..super::MAX_TEMPLATES)
                .map(|i| (format!("t{i}"), "[]".to_string()))
                .collect(),
            ..ChatData::default()
        };
        let text = settings_text(&data);
        assert!(
            text.chars().count() <= super::MAX_SETTINGS_CHARS,
            "{}",
            text.chars().count()
        );
        assert!(text.ends_with('…'), "{text}");
    }

    #[tokio::test]
    async fn template_limits_reject_unusable_names_bodies_and_overflow() {
        let sender = MockSender::scripted(vec![Outcome::MessageOk; 4], || api_error("boom"));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        let bot = Bot::new("42:TEST");
        let message = |reply: &str| {
            serde_json::from_value::<Message>(serde_json::json!({
                "message_id": 2,
                "date": 0,
                "chat": { "id": 1, "type": "private" },
                "from": { "id": 5, "is_bot": false, "first_name": "u" },
                "reply_to_message": {
                    "message_id": 1,
                    "date": 0,
                    "chat": { "id": 1, "type": "private" },
                    "text": reply,
                },
                "text": "/set_template x",
            }))
            .unwrap()
        };
        let short = message("before [] after");

        execute_command(&ctx, &bot, &short, Command::SetTemplate("漢".repeat(22)))
            .await
            .unwrap();
        assert!(sender.messages()[0].contains("name is too long"));
        assert!(stores.chat_store().get(1).await.template.is_empty());

        let long_body = message(&format!(
            "{} []",
            "<".repeat(super::MAX_TEMPLATE_BODY_CHARS)
        ));
        execute_command(&ctx, &bot, &long_body, Command::SetTemplate("long".into()))
            .await
            .unwrap();
        assert!(sender.messages()[1].contains("Template is too long"));
        assert!(stores.chat_store().get(1).await.template.is_empty());

        execute_command(&ctx, &bot, &short, Command::SetTemplate("ok".into()))
            .await
            .unwrap();
        stores
            .chat_store()
            .update(1, |data| {
                for i in 0..super::MAX_TEMPLATES - 1 {
                    data.template.insert(format!("t{i}"), "[]".into());
                }
            })
            .await
            .unwrap();
        execute_command(&ctx, &bot, &short, Command::SetTemplate("overflow".into()))
            .await
            .unwrap();
        assert!(sender.messages().last().unwrap().contains("maximum"));
        assert_eq!(
            stores.chat_store().get(1).await.template.len(),
            super::MAX_TEMPLATES
        );
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
