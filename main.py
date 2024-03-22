from aiohttp import ClientSession
from telegram import Update, Chat
from telegram.constants import ParseMode, ChatAction, ChatType
from telegram.ext import (
    Application,
    ApplicationBuilder,
    ContextTypes,
    Defaults,
    filters,
    InlineQueryHandler,
    PicklePersistence,
    MessageHandler,
    CommandHandler
)

import common
from tweet import TGTweet


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    query = update.inline_query.query
    if query == "":
        return
    async with TGTweet(context.bot_data['client'], query) as tweet:
        result = list(tweet.inline_query_generator)
        await update.inline_query.answer(result)


async def reply_media(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    await update.effective_chat.send_action(ChatAction.UPLOAD_PHOTO)
    # url = update.message.text.split(" ")[0]
    url = update.message.text
    async with TGTweet(context.bot_data['client'], url) as tweet:
        media = list(tweet.pm_media_generator)
        message_sent = await update.effective_message.reply_media_group(
            media,
            caption=tweet.message_text,
            reply_to_message_id=update.message.message_id
        )
    if 'forward_channel_id' in context.user_data:
        try:
            await update.effective_chat.copy_messages(
                chat_id=context.user_data['forward_channel_id'],
                message_ids=[m.id for m in message_sent],
            )
        except Exception as e:
            await update.effective_message.reply_text(str(e))


async def cmd_set_forward_channel(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    await update.effective_chat.send_action(ChatAction.TYPING)
    if not context.args:
        await update.effective_message.reply_text("Please provide a channel username or id.")
        return
    channel = context.args[0]
    try:
        channel: Chat = await context.bot.get_chat(channel)
    except Exception as e:
        await update.effective_message.reply_text(str(e))
        return
    if channel.type != ChatType.CHANNEL:
        await update.effective_message.reply_text("That is not a channel.")
        return
    try:
        channel_admin = await channel.get_administrators()
    except Exception as e:
        await update.effective_message.reply_text(str(e) + "\nPlease add the bot to the channel and set as admin")
        return
    user_bot = filter(lambda x: x.user.id == context.bot.id, channel_admin)
    user_bot = next(user_bot, None)
    if user_bot.can_post_messages:
        context.user_data['forward_channel_id'] = channel.id
        await update.effective_message.reply_text("Add successfully.")


async def cmd_remove_forward_channel(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    await update.effective_chat.send_action(ChatAction.TYPING)
    if 'forward_channel_id' in context.user_data:
        del context.user_data['forward_channel_id']
        await update.effective_message.reply_text("Remove successfully.")
        return
    await update.effective_message.reply_text("No channel to remove.")


async def post_init(application: Application) -> None:
    # commands = [
    #     BotCommand('start', CMD_START),
    # ]
    # await application.bot.set_my_commands(commands)
    DESCRIPTION = "A bot to fetch tweets from Twitter."
    await application.bot.set_my_description(DESCRIPTION)
    await application.bot.set_my_short_description(DESCRIPTION)
    application.bot_data['client'] = ClientSession()


async def post_stop(application: Application) -> None:
    await application.bot.send_message(common.ADMIN[0], "Shutting down...")


async def post_shutdown(application: Application) -> None:
    await application.bot_data['client'].close()


def main():
    defaults = Defaults(parse_mode=ParseMode.HTML, allow_sending_without_reply=True)
    persistence = PicklePersistence(filepath='pers_data')
    application = (ApplicationBuilder()
                   .token(common.BOT_TOKEN)
                   .defaults(defaults)
                   .persistence(persistence)
                   .post_init(post_init)
                   .post_stop(post_stop)
                   .post_shutdown(post_shutdown)
                   .build()
                   )

    # user_filter = filters.User()
    # user_filter.add_user_ids(common.admin)

    handlers = [
        MessageHandler(filters.Regex(common.x_url_regex) & filters.ChatType.PRIVATE, reply_media),
        InlineQueryHandler(inline_query, common.x_url_regex),
        CommandHandler("set_forward_channel", cmd_set_forward_channel),
        CommandHandler("remove_forward_channel", cmd_remove_forward_channel),
    ]

    application.add_handlers(handlers)

    if common.WEBHOOK:
        application.run_webhook(
            listen=common.WEBHOOK_LISTEN,
            port=common.WEBHOOK_PORT,
            secret_token=common.WEBHOOK_SECRET_TOKEN,
            key=common.WEBHOOK_KEY,
            cert=common.WEBHOOK_CERT,
            webhook_url=common.WEBHOOK_URL
        )
    else:
        application.run_polling()


if __name__ == '__main__':
    main()
