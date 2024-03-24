from functools import wraps

from aiohttp import ClientSession
from telegram import Update, Chat, InlineKeyboardMarkup, InlineKeyboardButton, ForceReply, Message
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
    CommandHandler, CallbackQueryHandler
)

import common
from tweet import TGTweet


def send_action(action):
    def decorator(func):
        @wraps(func)
        async def command_func(update: Update, context: ContextTypes.DEFAULT_TYPE, *args, **kwargs):
            await update.effective_chat.send_action(action)
            return await func(update, context, *args, **kwargs)

        return command_func

    return decorator


async def inline_query(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    query = update.inline_query.query
    if query == "":
        return
    common.logger.info(f"Query: {query}")
    async with TGTweet(query) as tweet:
        result = list(tweet.inline_query_generator)
        await update.inline_query.answer(result)


@send_action(ChatAction.UPLOAD_PHOTO)
async def url_media(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    url = update.message.text
    common.logger.info(f"Receiving url: {url}")
    async with TGTweet(url) as tweet:
        media = list(tweet.pm_media_generator)
        message_to_send = await update.effective_message.reply_media_group(
            media,
            caption=tweet.message_text,
            reply_to_message_id=update.message.message_id,
        )
        url = tweet.url
    if context.user_data.get('edit_before_forward', False):
        message_reply = await update.effective_message.reply_text(
            "Reply to edit message.",
            reply_markup=ForceReply(selective=True, input_field_placeholder="{}"),
        )
        context.user_data['message_reply'] = message_reply
        context.user_data['message_to_send'] = message_to_send
        context.user_data['message_url'] = url
        return
    if 'forward_channel_id' in context.user_data:
        await forward_message(update, context, message_to_send)


async def forward_message(
        update: Update,
        context: ContextTypes.DEFAULT_TYPE,
        message_sent: tuple[Message, ...],
) -> None:
    try:
        for i, m in enumerate(message_sent):
            await m.copy(context.user_data['forward_channel_id'])
    except Exception as e:
        await update.effective_message.reply_text(str(e))


async def edit_message(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if 'message_reply' not in context.user_data:
        return
    if update.message.reply_to_message != context.user_data['message_reply']:
        return
    update_text = update.message.text
    match = common.message_url_regex.search(update_text).span()
    if match:
        update_text = update_text[:match[0]] + '<a href="{0}">{1}</a>'.format(
            context.user_data['message_url'], update_text[match[0]:match[1]]
        ) + update_text[match[1]:]
    message_to_send = context.user_data['message_to_send']
    await message_to_send[0].edit_caption(
        update_text,
        reply_markup=InlineKeyboardMarkup.from_button(InlineKeyboardButton("✅ Forward", callback_data="forward"))
    )


async def query_forward_message(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    message_to_send = context.user_data['message_to_send']
    await forward_message(update, context, message_to_send)
    await update.callback_query.answer('Forwarded.')
    await update.callback_query.edit_message_reply_markup()
    del context.user_data['message_reply']
    del context.user_data['message_to_send']
    del context.user_data['message_url']


@send_action(ChatAction.TYPING)
async def cmd_set_forward_channel(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
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


@send_action(ChatAction.TYPING)
async def cmd_remove_forward_channel(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if 'forward_channel_id' in context.user_data:
        del context.user_data['forward_channel_id']
        await update.effective_message.reply_text("Remove successfully.")
        return
    await update.effective_message.reply_text("No channel to remove.")


@send_action(ChatAction.TYPING)
async def cmd_edit_before_forward(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if context.user_data.get('forward_channel_id', None) is None:
        await update.effective_message.reply_text("Please enable forward channel first.")
        return
    ebf_status = context.user_data.get('edit_before_forward', False)
    if ebf_status:
        context.user_data['edit_before_forward'] = False
        await update.effective_message.reply_text("Disable edit before forward.")
        return
    context.user_data['edit_before_forward'] = True
    await update.effective_message.reply_text("Enable edit before forward.")


async def post_init(application: Application) -> None:
    # commands = [
    #     BotCommand('start', CMD_START),
    # ]
    # await application.bot.set_my_commands(commands)
    DESCRIPTION = "A bot to fetch tweets from Twitter."
    await application.bot.set_my_description(DESCRIPTION)
    await application.bot.set_my_short_description(DESCRIPTION)
    TGTweet.set_session(ClientSession())


async def post_stop(application: Application) -> None:
    await application.bot.send_message(common.ADMIN[0], "Shutting down...")


async def post_shutdown(application: Application) -> None:
    await TGTweet.close_session()


def main():
    defaults = Defaults(parse_mode=ParseMode.HTML, allow_sending_without_reply=True)
    persistence = PicklePersistence(filepath='data/pers.pkl')
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
        MessageHandler(filters.Regex(common.x_url_regex) & filters.ChatType.PRIVATE, url_media),
        InlineQueryHandler(inline_query, common.x_url_regex),
        CommandHandler("set_forward_channel", cmd_set_forward_channel),
        CommandHandler("remove_forward_channel", cmd_remove_forward_channel),
        CommandHandler("edit_before_forward", cmd_edit_before_forward),
        MessageHandler(~filters.COMMAND & filters.ChatType.PRIVATE, edit_message),
        CallbackQueryHandler(query_forward_message, pattern="forward"),
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
