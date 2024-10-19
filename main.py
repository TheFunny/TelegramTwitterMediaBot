from __future__ import annotations

import html
from functools import wraps
from typing import TYPE_CHECKING

from telegram import InlineKeyboardButton, InlineKeyboardMarkup, MessageEntity
from telegram.constants import ChatAction, ChatType, ParseMode
from telegram.ext import (ApplicationBuilder, CallbackQueryHandler, CommandHandler, ContextTypes, Defaults,
                          InlineQueryHandler, MessageHandler, PicklePersistence, filters)

import common
import utils.regex as regex
from utils.context import ChatData, CustomContext, EditMessage
from utils.logger import get_logger
from utils.net import NetClient
from utils.pixiv import ProcessPixiv
from utils.telegram import Telegram

if TYPE_CHECKING:
    from telegram import Message, Update
    from telegram.ext import Application

logger = get_logger(__name__)


def send_action(action):
    def decorator(func):
        @wraps(func)
        async def command_func(update: Update, context: CustomContext, *args, **kwargs):
            try:
                await update.effective_chat.send_action(action)
            finally:
                return await func(update, context, *args, **kwargs)

        return command_func

    return decorator


def extract_urls(message: Message) -> set[str]:
    types = [MessageEntity.URL, MessageEntity.TEXT_LINK]
    res = message.parse_entities(types)
    res.update(message.parse_caption_entities(types))
    res.update({key: key.url for key in res if key.type == MessageEntity.TEXT_LINK})
    return set(res.values())


async def inline_query(update: Update, context: CustomContext) -> None:
    query = update.inline_query.query
    if query == "":
        return
    logger.info(f"Query: {query}")
    async with Telegram(query) as tweet:
        if not tweet:
            return
        await update.inline_query.answer(tweet.inline_query_result())


@send_action(ChatAction.UPLOAD_PHOTO)
async def url_media(update: Update, context: CustomContext, url: str) -> None:
    async with Telegram(url) as tweet:
        if not tweet:
            return
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
    if context.chat_data.edit_before_forward:
        message_reply = await update.effective_message.reply_text(
            "Reply to edit message.",
            reply_markup=InlineKeyboardMarkup.from_column(
                [InlineKeyboardButton(name, callback_data=f"template|{name}") for name in
                 context.chat_data.template.keys()] + [InlineKeyboardButton("↩️ Confirm", callback_data="forward")]
            ),
            reply_to_message_id=update.message.message_id,
        )
        context.chat_data.edit_message[message_reply.id] = EditMessage(
            url=url,
            forward=message_to_send
        )
        return
    if context.chat_data.forward_channel_id:
        await forward_message(update, context, message_to_send)


async def handel_url_media(update: Update, context: CustomContext) -> None:
    url = update.message.text
    logger.info(f"Receiving url: {url}")
    await url_media(update, context, url)


async def forward_message(
        update: Update,
        context: CustomContext,
        message_to_send: tuple[Message, ...],
) -> None:
    try:
        await update.effective_chat.copy_messages(
            context.chat_data.forward_channel_id,
            [m.id for m in message_to_send]
        )
    except Exception as e:
        await update.effective_message.reply_text(str(e))


async def edit_message(update: Update, context: CustomContext) -> bool:
    if not (reply := update.message.reply_to_message):
        return False
    _edit_message = context.chat_data.edit_message.get(reply.id, None)
    if not _edit_message:
        return False
    new_text = '<a href="{0}">{1}</a>'.format(
        _edit_message.url,
        html.escape(update.message.text)
    )
    update_text = context.chat_data.template[template].replace("[]", new_text) if (
        template := _edit_message.template) else new_text
    await _edit_message.forward[0].edit_caption(update_text)
    return True


async def handle_message(update: Update, context: CustomContext) -> None:
    if await edit_message(update, context):
        return
    if not (urls := extract_urls(update.message)):
        return
    for url in urls:
        await url_media(update, context, url)


async def query_forward_message(update: Update, context: CustomContext) -> None:
    _edit_message = context.chat_data.edit_message[update.effective_message.id]
    await forward_message(update, context, _edit_message.forward)
    await update.callback_query.answer('✅ Forwarded')
    await update.callback_query.delete_message()
    del _edit_message


async def query_template(update: Update, context: CustomContext) -> None:
    query = update.callback_query
    await query.answer()
    name = query.data.split("|")[1]
    _edit_message = context.chat_data.edit_message[query.message.message_id]
    _edit_message.template = name
    await _edit_message.forward[0].edit_caption(context.chat_data.template[name])


@send_action(ChatAction.TYPING)
async def cmd_set_forward_channel(update: Update, context: CustomContext) -> None:
    if not context.args:
        await update.effective_message.reply_text("Please provide a channel username or id.")
        return
    channel = context.args[0]
    try:
        channel = await context.bot.get_chat(channel)
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
    user = filter(lambda x: x.user.id == update.effective_user.id, channel_admin)
    user = next(user, None)
    if not user:
        await update.effective_message.reply_text("You are not an admin of the channel.")
        return
    user_bot = filter(lambda x: x.user.id == context.bot.id, channel_admin)
    user_bot = next(user_bot, None)
    if user_bot.can_post_messages:
        context.chat_data.forward_channel_id = channel.id
        await update.effective_message.reply_text("Add successfully.")


@send_action(ChatAction.TYPING)
async def cmd_remove_forward_channel(update: Update, context: CustomContext) -> None:
    if context.chat_data.forward_channel_id:
        context.chat_data.forward_channel_id = None
        await update.effective_message.reply_text("Remove successfully.")
        return
    await update.effective_message.reply_text("No channel to remove.")


@send_action(ChatAction.TYPING)
async def cmd_edit_before_forward(update: Update, context: CustomContext) -> None:
    if context.chat_data.forward_channel_id is None:
        await update.effective_message.reply_text("Please enable forward channel first.")
        return
    if context.chat_data.edit_before_forward:
        context.chat_data.edit_before_forward = False
        context.chat_data.edit_message.clear()
        await update.effective_message.reply_text("Disable edit before forward.")
        return
    context.chat_data.edit_before_forward = True
    await update.effective_message.reply_text("Enable edit before forward.")


@send_action(ChatAction.TYPING)
async def cmd_set_template(update: Update, context: CustomContext) -> None:
    reply = update.effective_message.reply_to_message
    if not reply:
        await update.effective_message.reply_text("Please reply to a message to set as template.")
        return
    if '[]' not in (template := reply.text_html):
        await update.effective_message.reply_text("Please reply to a message with [] to set as template.")
        return
    if not context.args:
        await update.effective_message.reply_text("Please provide a name for the template.")
        return
    context.chat_data.template[''.join(context.args)] = template
    await update.effective_message.reply_text("Template set.")


@send_action(ChatAction.TYPING)
async def cmd_user_dict(update: Update, context: CustomContext) -> None:
    await update.effective_message.reply_text(html.escape(str(context.chat_data)), disable_web_page_preview=True)


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
    if common.ADMIN:
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
                   .context_types(ContextTypes(context=CustomContext, chat_data=ChatData))
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
        InlineQueryHandler(inline_query),
        MessageHandler((filters.Regex(regex.x_url) | filters.Regex(regex.pixiv_url)) & filters.ChatType.PRIVATE,
                       handel_url_media),
        CommandHandler("set_forward_channel", cmd_set_forward_channel),
        CommandHandler("remove_forward_channel", cmd_remove_forward_channel),
        CommandHandler("edit_before_forward", cmd_edit_before_forward),
        CommandHandler("set_template", cmd_set_template),
        MessageHandler(~filters.COMMAND & filters.ChatType.PRIVATE, handle_message),
        CallbackQueryHandler(query_forward_message, pattern="forward"),
        CallbackQueryHandler(query_template, pattern=r"^template\|"),
        CommandHandler("bot_dict", cmd_user_dict),
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
