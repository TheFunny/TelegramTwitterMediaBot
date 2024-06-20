from typing import Type, TypedDict

from telegram import InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, InputMediaPhoto, \
    InputMediaVideo

TypeInlineQueryResult: Type[
    InlineQueryResultMpeg4Gif | InlineQueryResultPhoto | InlineQueryResultVideo] = InlineQueryResultMpeg4Gif | InlineQueryResultPhoto | InlineQueryResultVideo
InputMediaAnimation = tuple[str, bool]
TypeMessageMediaResult: Type[
    InputMediaPhoto | InputMediaVideo | InputMediaAnimation] = InputMediaPhoto | InputMediaVideo | InputMediaAnimation


class TweetInfo(TypedDict):
    tweetID: str
    user_name: str
    user_screen_name: str
    text: str
    media_extended: list[dict]
    possibly_sensitive: bool
