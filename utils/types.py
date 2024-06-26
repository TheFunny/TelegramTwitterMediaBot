from typing import TypedDict

from telegram import InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, InputMediaPhoto, \
    InputMediaVideo

TypeInlineQueryResult = InlineQueryResultMpeg4Gif | InlineQueryResultPhoto | InlineQueryResultVideo
InputMediaAnimation = tuple[str, bool]
TypeMessageMediaResult = InputMediaPhoto | InputMediaVideo | InputMediaAnimation


class TweetInfo(TypedDict):
    tweetID: str
    user_name: str
    user_screen_name: str
    text: str
    media_extended: list[dict]
    possibly_sensitive: bool
