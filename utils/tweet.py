from __future__ import annotations

import html
from functools import cached_property
from typing import Generator, TYPE_CHECKING
from uuid import uuid4

from telegram import (InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, InputMediaPhoto,
                      InputMediaVideo)

from common import get_logger, x_media_regex, x_tco_regex, x_url_regex
from .net import NetClient

if TYPE_CHECKING:
    from .types import TweetInfo, TypeInlineQueryResult, TypeMessageMediaResult

logger = get_logger(__name__)


twimg_url = 'https://pbs.twimg.com/'
vx_api_url = 'https://api.vxtwitter.com/{0}/status/{1}'

message_raw_text = """{url}
<a href="{author_url}">{author}</a>: {text}
"""


class TweetMedia:
    __slots__ = ('_url', '_thumb', '_type', '__dict__')

    def __init__(self, url: str, thumb: str, media_type: str):
        self._url: str = url
        self._thumb: str = thumb
        self._type: str = media_type

    def __str__(self):
        return f"TweetMedia(url={self.url}, thumb={self.thumb}, type={self.type})"

    @cached_property
    def _uri(self) -> str | None:
        if match := x_media_regex.match(self._url):
            return match.group(2).removesuffix('.jpg').removesuffix('.png')
        return None

    @cached_property
    def url(self) -> str:
        match self._type:
            case "image":
                return f"{twimg_url}{self._uri}?format=jpg&name=4096x4096"
            case "video":
                return self._url
            case "gif":
                return self._url
            case _:
                return self._url

    @cached_property
    def thumb(self) -> str:
        match self._type:
            case "image":
                return f"{twimg_url}{self._uri}?format=jpg&name=thumb"
            case "video":
                return self._thumb
            case "gif":
                return self._thumb
            case _:
                return self._thumb

    @property
    def type(self) -> str:
        return self._type


class Tweet:
    __slots__ = ('_id', '_author', '_author_id', '_text', '_media', '_sensitive', '__dict__')

    def __init__(
            self,
            tweet_id: str,
            author: str,
            author_id: str,
            text: str,
            media: list[TweetMedia],
            sensitive: bool = False
    ):
        self._id: str = tweet_id
        self._author: str = author
        self._author_id: str = author_id
        self._text: str = text
        self._media: list[TweetMedia] = media
        self._sensitive: bool = sensitive

    @property
    def id(self) -> str:
        return self._id

    @cached_property
    def url(self) -> str:
        return f"https://twitter.com/{self._author_id}/status/{self._id}"

    @property
    def author(self) -> str:
        return self._author

    @cached_property
    def author_url(self) -> str:
        return f"https://twitter.com/{self._author_id}"

    @property
    def text(self) -> str:
        return self._text

    @property
    def media(self) -> list[TweetMedia]:
        return self._media

    @property
    def sensitive(self) -> bool:
        return self._sensitive


class ProcessTweet:
    __slots__ = ('_url', '_tweet')

    def __init__(self, url: str):
        self._url: str = url

    async def __aenter__(self):
        self._tweet = await self._fetch_tweet()
        return Tweet(
            tweet_id=self._tweet["tweetID"],
            author=self._tweet["user_name"],
            author_id=self._tweet["user_screen_name"],
            text=self._tweet_text,
            media=self._tweet_media,
            sensitive=self._tweet["possibly_sensitive"]
        )

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    async def _fetch_tweet(self) -> TweetInfo:
        match = x_url_regex.match(self._url)
        assert match, f"Invalid URL: {self._url}"
        auther_id, tweet_id = match.groups()
        return await NetClient.fetch_json(vx_api_url.format(auther_id, tweet_id))

    @property
    def _tweet_text(self) -> str:
        match = x_tco_regex.search(self._tweet['text'])
        return self._tweet['text'][:match.start()].strip(" ") if match else self._tweet['text']

    @property
    def _tweet_media(self) -> list[TweetMedia]:
        return [
            TweetMedia(
                url=tweet_media['url'],
                thumb=tweet_media['thumbnail_url'],
                media_type=tweet_media['type']
            )
            for tweet_media in self._tweet['media_extended']
        ]


class TelegramTweet:
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
        return message_raw_text.format(
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
