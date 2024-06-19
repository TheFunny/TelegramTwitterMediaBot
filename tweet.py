from __future__ import annotations

import html
from functools import cached_property
from typing import TYPE_CHECKING
from uuid import uuid4

from httpx import AsyncClient
from telegram import (InlineQueryResultMpeg4Gif, InlineQueryResultPhoto, InlineQueryResultVideo, InputMediaPhoto,
                      InputMediaVideo)

from common import get_logger, x_media_regex, x_tco_regex, x_url_regex

if TYPE_CHECKING:
    from typing import Generator, TypedDict

logger = get_logger(__name__)


twimg_url = 'https://pbs.twimg.com/'
vx_api_url = 'https://api.vxtwitter.com/{0}/status/{1}'

message_raw_text = """{url}
<a href="{author_url}">{author}</a>: {text}
"""


def create_client() -> AsyncClient:
    return AsyncClient(http2=True)


async def close_client(_client: AsyncClient) -> None:
    await _client.aclose()


async def fetch_json(_client: AsyncClient, url: str) -> dict:
    logger.info(f"Fetching {url}")
    response = await _client.get(url)
    assert response.status_code == response.is_success, f"Failed to fetch {url}, status code {response.status_code}"
    return response.json()


class TweetMedia:
    def __init__(self, url: str, thumb: str, media_type: str):
        self._url: str = url
        self._thumb: str = thumb
        self._type: str = media_type

    def __str__(self):
        return f"Media[url: {self.url} thumb: {self.thumb} type: {self.type}]"

    @property
    def _uri(self) -> str | None:
        match = x_media_regex.match(self._url)
        if match:
            return match.group(2).removesuffix('.jpg').removesuffix('.png')
        return None

    @property
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

    @property
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

    @property
    def url(self) -> str:
        return f"https://twitter.com/{self._author_id}/status/{self._id}"

    @property
    def author(self) -> str:
        return self._author

    @property
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


class TGTweet(Tweet):
    _httpx_client: AsyncClient

    def __init__(self, url: str):
        self._url: str = url
        self._api_param: tuple[str] = self._tweet_id
        self._is_single_gif: bool = False
        assert self._api_param

    async def __aenter__(self):
        self._tweet: dict = await self._fetch_tweet(self._api_param)
        super().__init__(*self._init_properties)
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    @classmethod
    def init_client(cls) -> None:
        cls._httpx_client = create_client()

    @classmethod
    async def close_client(cls) -> None:
        await close_client(cls._httpx_client)

    async def _fetch_tweet(self, api_param: tuple[str]) -> dict:
        return await fetch_json(self._httpx_client, vx_api_url.format(*api_param))

    @property
    def _tweet_id(self) -> tuple[str] | None:
        match = x_url_regex.match(self._url)
        if match:
            return match.groups()
        return None

    @property
    def _tweet_text(self) -> str:
        match = x_tco_regex.search(self._tweet['text'])
        return self._tweet['text'][:match.start()].strip(" ") if match else self._tweet['text']

    @property
    def _tweet_media(self) -> list[TweetMedia]:
        return [
            TweetMedia(
                url=x['url'],
                thumb=x['thumbnail_url'],
                media_type=x['type']
            )
            for x in self._tweet['media_extended']
        ]

    @property
    def _init_properties(self) -> tuple:
        id = self._tweet['tweetID']
        author = self._tweet['user_name']
        author_id = self._tweet['user_screen_name']
        text = self._tweet_text
        media = self._tweet_media
        sensitive = self._tweet['possibly_sensitive']
        return id, author, author_id, text, media, sensitive

    @property
    def is_single_gif(self) -> bool:
        return self._is_single_gif

    @property
    def message_text(self) -> str:
        return message_raw_text.format(
            url=self.url,
            author_url=self.author_url,
            author=html.escape(self.author),
            text=html.escape(self.text)
        )

    @property
    def inline_query_generator(self) -> Generator[
        InlineQueryResultPhoto | InlineQueryResultVideo | InlineQueryResultMpeg4Gif, None, None
    ]:
        for tweet_media in self.media:
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
                    title=self.text,
                    caption=self.message_text
                )
            elif tweet_media.type == "gif":
                yield InlineQueryResultMpeg4Gif(
                    id=str(uuid4()),
                    mpeg4_url=tweet_media.url,
                    thumbnail_url=tweet_media.thumb,
                    caption=self.message_text
                )

    @property
    def pm_media_generator(self) -> Generator[InputMediaPhoto | InputMediaVideo | tuple[str, bool], None, None]:
        for tweet_media in self.media:
            logger.info(str(tweet_media))
            if tweet_media.type == "image":
                yield InputMediaPhoto(
                    media=tweet_media.url,
                    has_spoiler=self.sensitive
                )
            elif tweet_media.type == "video":
                yield InputMediaVideo(
                    media=tweet_media.url,
                    has_spoiler=self.sensitive,
                    thumbnail=tweet_media.thumb
                )
            elif tweet_media.type == "gif":
                if len(self.media) == 1:
                    self._is_single_gif = True
                    yield tweet_media.url, self.sensitive
                yield InputMediaVideo(
                    media=tweet_media.url,
                    has_spoiler=self.sensitive,
                    thumbnail=tweet_media.thumb
                )
