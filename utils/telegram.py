from __future__ import annotations

import html
from functools import cached_property
from typing import Generator, TYPE_CHECKING
from uuid import uuid4

from telegram import InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, InputMediaPhoto, \
    InputMediaVideo

from common import PIXIV_REFRESH_TOKEN
from .logger import get_logger
from .pixiv import ProcessPixiv
from .regex import pixiv_url, x_url
from .tweet import ProcessTweet

if TYPE_CHECKING:
    from .types import TypeInlineQueryResult, TypeMessageMediaResult

logger = get_logger(__name__)

message_raw_text_tweet = """{url}
<a href="{author_url}">{author}</a>: {text}
"""

message_raw_text_pixiv = """<a href="{url}">{text}</a> / <a href="{author_url}">{author}</a>
{tags}
"""


class Telegram:
    def __init__(self, url: str):
        self._url = url

    async def __aenter__(self):
        if x_url.match(self._url):
            async with TelegramTweet(self._url) as tweet:
                return tweet
        elif PIXIV_REFRESH_TOKEN and pixiv_url.match(self._url):
            async with TelegramPixiv(self._url) as pixiv:
                return pixiv
        else:
            return None  # TODO add raise and catch

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass


class TelegramTweet:
    message_raw_text = message_raw_text_tweet
    __slots__ = ('_url', '_tweet', '__dict__')

    def __init__(self, url: str):
        self._url: str = url

    async def __aenter__(self):
        async with ProcessTweet(self._url) as tweet:
            self._tweet = tweet
            return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    @cached_property
    def url(self) -> str:
        return self._tweet.url

    @cached_property
    def message_text(self) -> str:
        tweet = self._tweet
        return self.message_raw_text.format(
            url=tweet.url,
            author_url=tweet.author_url,
            author=html.escape(tweet.author),
            text=html.escape(tweet.text)
        )

    def inline_query_result(self) -> tuple[TypeInlineQueryResult, ...]:
        return tuple(self.inline_query_generator())

    def message_media_result(self) -> tuple[TypeMessageMediaResult, ...]:
        return tuple(self.message_media_generator())

    def inline_query_generator(self) -> Generator[TypeInlineQueryResult, None, None]:
        tweet = self._tweet
        for tweet_media in tweet.media:
            logger.info(str(tweet_media))
            if tweet_media.type == "image":
                yield InlineQueryResultPhoto(
                    id=str(uuid4()),
                    photo_url=tweet_media.url,
                    thumbnail_url=tweet_media.thumb,
                    caption=self.message_text
                )
            elif tweet_media.type == "video":
                yield InlineQueryResultVideo(
                    id=str(uuid4()),
                    video_url=tweet_media.url,
                    mime_type="video/mp4",
                    thumbnail_url=tweet_media.thumb,
                    title=tweet.text,
                    caption=self.message_text
                )
            elif tweet_media.type == "gif":
                yield InlineQueryResultMpeg4Gif(
                    id=str(uuid4()),
                    mpeg4_url=tweet_media.url,
                    thumbnail_url=tweet_media.thumb,
                    caption=self.message_text
                )

    def message_media_generator(self) -> Generator[TypeMessageMediaResult, None, None]:
        tweet = self._tweet
        for tweet_media in tweet.media:
            logger.info(str(tweet_media))
            if tweet_media.type == "image":
                yield InputMediaPhoto(
                    media=tweet_media.url,
                    has_spoiler=tweet.sensitive
                )
            elif tweet_media.type == "video":
                yield InputMediaVideo(
                    media=tweet_media.url,
                    has_spoiler=tweet.sensitive,
                    thumbnail=tweet_media.thumb
                )
            elif tweet_media.type == "gif":
                if len(tweet.media) == 1:
                    yield tweet_media.url, tweet.sensitive
                yield InputMediaVideo(
                    media=tweet_media.url,
                    has_spoiler=tweet.sensitive,
                    thumbnail=tweet_media.thumb
                )


class TelegramPixiv:
    message_raw_text = message_raw_text_pixiv
    __slots__ = ('_url', '_pixiv')

    def __init__(self, url: str):
        self._url = url

    async def __aenter__(self):
        async with ProcessPixiv(self._url) as pixiv:
            self._pixiv = pixiv
            return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    @property
    def url(self) -> str:
        return self._pixiv.url

    @property
    def message_text(self) -> str:
        pixiv = self._pixiv
        return self.message_raw_text.format(
            url=pixiv.url,
            author_url=pixiv.author_url,
            author=html.escape(pixiv.author),
            text=html.escape(pixiv.title),
            tags=html.escape(" ".join(f"#{name}" for name in pixiv.tags))
        )

    def inline_query_result(self) -> tuple[TypeInlineQueryResult, ...]:
        return tuple(self.inline_query_generator())

    def message_media_result(self) -> tuple[TypeMessageMediaResult, ...]:
        return tuple(self.message_media_generator())

    def inline_query_generator(self) -> Generator[TypeInlineQueryResult, None, None]:
        pixiv = self._pixiv
        for media in pixiv.images:
            logger.info(str(media))
            if pixiv.type in ("illust", "manga"):
                yield InlineQueryResultPhoto(
                    id=str(uuid4()),
                    photo_url=media.large,
                    thumbnail_url=media.thumb,
                    caption=self.message_text
                )
            else:
                yield

    def message_media_generator(self) -> Generator[TypeMessageMediaResult, None, None]:
        pixiv = self._pixiv
        for media in pixiv.images:
            logger.info(str(media))
            if pixiv.type in ("illust", "manga"):
                yield InputMediaPhoto(
                    media=media.large,
                    has_spoiler=pixiv.is_nsfw
                )
            else:
                yield
