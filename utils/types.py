from __future__ import annotations

from typing import Literal, TypedDict

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


class BskyInfo(TypedDict):
    thread: BskyThread


class BskyThread(TypedDict):
    post: BskyPost


class BskyPost(TypedDict):
    author: BskyAuthor
    record: BskyPostRecord
    embed: BskyEmbedImages | BskyEmbedVideo | BskyEmbedExternal
    labels: list[BskyLabel]


class BskyAuthor(TypedDict):
    handle: str
    displayName: str


class BskyPostRecord(TypedDict):
    text: str


class BskyLabel(TypedDict):
    val: str


class BskyEmbedImage(TypedDict):
    thumb: str
    fullsize: str


BskyEmbedImages = TypedDict('BskyEmbedImages', {
    '$type': Literal['app.bsky.embed.images#view'],
    'images': list[BskyEmbedImage]
})

BskyEmbedVideo = TypedDict('BskyEmbedVideo', {
    '$type': Literal['app.bsky.embed.video#view'],
    'playlist': str,
    'thumbnail': str
})


class BskyEmbedExternalItem(TypedDict):
    uri: str
    thumb: str


BskyEmbedExternal = TypedDict('BskyEmbedExternal', {
    '$type': Literal['app.bsky.embed.external#view'],
    'external': BskyEmbedExternalItem
})
