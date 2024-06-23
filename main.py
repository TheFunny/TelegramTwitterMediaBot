from __future__ import annotations

import html
from functools import wraps
from typing import TYPE_CHECKING

from telegram import InlineKeyboardButton, InlineKeyboardMarkup
from telegram.constants import ChatAction, ChatType, ParseMode
from telegram.ext import (ApplicationBuilder, CallbackQueryHandler, CommandHandler, Defaults,
                          InlineQueryHandler, MessageHandler, PicklePersistence, filters)

import common
import utils.regex as regex
from utils.logger import get_logger
from utils.net import NetClient
from utils.pixiv import ProcessPixiv
from utils.telegram import Telegram

if TYPE_CHECKING:
    from telegram import Chat, Message, Update
    from telegram.ext import Application, ContextTypes

logger = get_logger(__name__)


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
    logger.info(f"Query: {query}")
    async with Telegram(query) as tweet:
        await update.inline_query.answer(tweet.inline_query_result())


@send_action(ChatAction.UPLOAD_PHOTO)
async def url_media(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    url = update.message.text
    logger.info(f"Receiving url: {url}")
    async with Telegram(url) as tweet:
        media = tweet.message_media_result()
        if not media:
            await update.effective_message.reply_text("No media found or media type is not supported.")
            return
        message_to_send = await update.effective_message.reply_media_group(
            media,
            caption=tweet.message_text,
            reply_to_message_id=update.message.message_id,
        ) if not isinstance(media[0], tuple) else await update.effective_message.reply_animation(
            media[0][0],
            caption=tweet.message_text,
            reply_to_message_id=update.message.message_id,
            has_spoiler=media[0][1]
        )
        if not isinstance(message_to_send, tuple):
            message_to_send = (message_to_send,)
        url = tweet.url
    if context.user_data.get('edit_before_forward', False):
        message_reply = await update.effective_message.reply_text(
            "Reply to edit message. [URL]",
            reply_markup=InlineKeyboardMarkup.from_button(
                InlineKeyboardButton("↩️ Confirm", callback_data="forward")
            ),
            reply_to_message_id=update.message.message_id,
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
        message_to_send: tuple[Message, ...],
) -> None:
    try:
        await update.effective_chat.copy_messages(
            context.user_data['forward_channel_id'],
            [m.id for m in message_to_send]
        )
    except Exception as e:
        await update.effective_message.reply_text(str(e))


async def edit_message(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    if 'message_reply' not in context.user_data:
        return
    if update.message.reply_to_message != context.user_data['message_reply']:
        return
    template = context.user_data.get('template', None)
    message_url = '<a href="{0}">{1}</a>'
    url = context.user_data['message_url']
    if template:
        update_text = template.replace("[]", message_url.format(
            url,
            html.escape(update.message.text)
        ))
    else:
        update_text = html.escape(update.message.text)
        match = regex.message_url.search(update_text)
        if match:
            match = match.span()
            update_text = update_text[:match[0]] + message_url.format(
                url,
                update_text[match[0] + 1:match[1] - 1]
            ) + update_text[match[1]:]
    message_to_send = context.user_data['message_to_send']
    await message_to_send[0].edit_caption(update_text)


async def query_forward_message(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    message_to_send = context.user_data['message_to_send']
    await forward_message(update, context, message_to_send)
    await update.callback_query.answer('✅ Forwarded')
    await update.callback_query.delete_message()
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
        context.user_data.pop('message_reply', None)
        context.user_data.pop('message_to_send', None)
        context.user_data.pop('message_url', None)
        await update.effective_message.reply_text("Disable edit before forward.")
        return
    context.user_data['edit_before_forward'] = True
    await update.effective_message.reply_text("Enable edit before forward.")


@send_action(ChatAction.TYPING)
async def cmd_set_template(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    reply = update.effective_message.reply_to_message
    if not reply:
        await update.effective_message.reply_text("Please reply to a message to set as template.")
        return
    if '[]' not in reply.text_html:
        await update.effective_message.reply_text("Please reply to a message with [] to set as template.")
        return
    context.user_data['template'] = reply.text_html
    await update.effective_message.reply_text("Template set.")


@send_action(ChatAction.TYPING)
async def cmd_user_dict(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    await update.effective_message.reply_text(str(context.user_data))


async def post_init(application: Application) -> None:
    # commands = [
    #     BotCommand('start', CMD_START),
    # ]
    # await application.bot.set_my_commands(commands)
    DESCRIPTION = "A bot to fetch tweets from Twitter."
    await application.bot.set_my_description(DESCRIPTION)
    await application.bot.set_my_short_description(DESCRIPTION)
    NetClient.init_client()
    if common.PIXIV_REFRESH_TOKEN:
        await ProcessPixiv.init_client(common.PIXIV_REFRESH_TOKEN)


async def post_stop(application: Application) -> None:
    await application.bot.send_message(common.ADMIN[0], "Shutting down...")


async def post_shutdown(application: Application) -> None:
    await NetClient.close_client()


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
                   .concurrent_updates(True)
                   .http_version('2')
                   .build()
                   )

    user_filter = filters.User()
    user_filter.add_user_ids(common.ADMIN)

    handlers = [
        InlineQueryHandler(inline_query, regex.x_url),
        MessageHandler(filters.Regex(regex.x_url) & filters.ChatType.PRIVATE, url_media),
        CommandHandler("set_forward_channel", cmd_set_forward_channel),
        CommandHandler("remove_forward_channel", cmd_remove_forward_channel),
        CommandHandler("edit_before_forward", cmd_edit_before_forward),
        CommandHandler("set_template", cmd_set_template),
        MessageHandler(~filters.COMMAND & filters.ChatType.PRIVATE, edit_message),
        CallbackQueryHandler(query_forward_message, pattern="forward"),
        CommandHandler("bot_dict", cmd_user_dict, filters=user_filter),
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
