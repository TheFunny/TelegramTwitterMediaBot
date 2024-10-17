use teloxide::types::{Message, Recipient, ReplyParameters};
use teloxide::utils::command::BotCommands;
use teloxide::{prelude::*, RequestError};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "snake_case", description = "")]
enum Command {
    #[command(description = "")]
    Start,
    #[command(description = "")]
    Help,
    #[command(description = "", parse_with = "split")]
    SetForwardChannel(String),
    #[command(description = "")]
    RemoveForwardChannel,
    #[command(description = "")]
    EditBeforeForward,
    #[command(description = "", parse_with = "split")]
    SetTemplate(String),
}

async fn reply<T>(bot: Bot, message: Message, text: T) -> Result<Message, RequestError>
where
    T: Into<String>,
{
    bot.send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id).allow_sending_without_reply())
        .await
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
) -> Result<String, SetForwardChannelError> {
    if channel.is_empty() {
        return Err(SetForwardChannelError::EmptyParameter);
    }
    let channel = match channel.parse::<i64>() {
        Ok(id) => Recipient::Id(ChatId(id)),
        Err(_) => Recipient::ChannelUsername(channel),
    };
    log::info!(
        "Set forward channel for {} ({}) to {}",
        message.from.clone().unwrap().full_name(),
        message.chat.id,
        channel
    );
    let channel_title = match bot.get_chat(channel.clone()).await {
        Err(e) => {
            log::error!("Failed to get channel {}: {}", channel, e);
            return Err(SetForwardChannelError::NotBotAdmin(e));
        }
        Ok(chat) => {
            if !chat.is_channel() {
                return Err(SetForwardChannelError::NotChannel);
            }
            chat.title().unwrap().to_string()
        }
    };
    match bot.get_chat_administrators(channel.clone()).await {
        Err(e) => {
            log::error!("Failed to get channel administrators {}: {}", channel, e);
            return Err(SetForwardChannelError::NotBotAdmin(e));
        }
        Ok(chat) => {
            if !chat.iter().any(|admin| admin.user.id == message.chat.id) {
                return Err(SetForwardChannelError::NotAdmin);
            }
            let bot_id = bot.get_me().await.expect("Failed get bot id").user.id;
            if let Some(bot_user) = chat.iter().find(|&admin| admin.user.id == bot_id) {
                if !bot_user.can_post_messages() {
                    return Err(SetForwardChannelError::NotBotCanPost);
                }
            }
        }
    }
    Ok(channel_title)
}

pub async fn message_handler(bot: Bot, message: Message) -> Result<(), RequestError> {
    let text = message.text();
    if text.is_none() {
        log::info!("Received a message without text");
        return respond(());
    }
    bot.send_chat_action(message.chat.id, teloxide::types::ChatAction::Typing)
        .await?;
    if let Ok(command) = Command::parse(text.unwrap(), "") {
        match command {
            Command::Start => {
                bot.send_message(message.chat.id, "Hello!").await?;
            }
            Command::Help => {
                bot.send_message(message.chat.id, Command::descriptions().to_string())
                    .await?;
            }
            Command::SetForwardChannel(channel) => {
                let result = match set_forward_channel_handler(&bot, &message, channel).await {
                    Ok(title) => format!("Set forward channel to {}", title),
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
                reply(bot, message, result).await?;
            }
            Command::RemoveForwardChannel => {
                log::info!(
                    "Remove forward channel for {} ({})",
                    message.from.unwrap().full_name(),
                    message.chat.id
                );
            }
            Command::EditBeforeForward => {
                log::info!(
                    "Enable edit before forward for {} ({})",
                    message.from.unwrap().full_name(),
                    message.chat.id
                );
            }
            Command::SetTemplate(template) => {
                log::info!(
                    "Set template {} for {} ({})",
                    template,
                    message.from.unwrap().full_name(),
                    message.chat.id
                );
            }
        }
    }
    respond(())
}
