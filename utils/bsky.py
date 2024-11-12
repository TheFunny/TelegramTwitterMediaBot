from __future__ import annotations

from functools import cached_property
from typing import TYPE_CHECKING

from utils.net import NetClient
from utils.regex import bsky_url

if TYPE_CHECKING:
    from utils.types import BskyEmbedImages, BskyInfo, BskyEmbedVideo, BskyEmbedExternal

bsky_api_url = 'https://public.api.bsky.app/xrpc/app.bsky.feed.getPostThread'

SENSITIVE_TAG = {'sexual', 'nudity', 'porn', 'graphic-media'}

class BskyMedia:
    __slots__ = ('_url', '_thumb', '_type', '__dict__')

    def __init__(self, url: str, thumb: str, media_type: str):
        self._url: str = url
        self._thumb: str = thumb
        self._type: str = media_type

    def __str__(self):
        return f"BskyMedia(url={self.url}, thumb={self.thumb}, type={self.type})"

    @property
    def url(self) -> str:
        return self._url

    @property
    def thumb(self) -> str:
        return self._thumb

    @property
    def type(self) -> str:
        return self._type


class Bsky:
    __slots__ = ('_id', '_author', '_author_id', '_text', '_media', '_sensitive', '__dict__')

    def __init__(
            self,
            id: str,
            author: str,
            author_id: str,
            text: str,
            media: list[BskyMedia],
            sensitive: bool = False
    ):
        self._id: str = id
        self._author: str = author
        self._author_id: str = author_id
        self._text: str = text
        self._media: list[BskyMedia] = media
        self._sensitive: bool = sensitive

    @property
    def id(self) -> str:
        return self._id

    @cached_property
    def url(self) -> str:
        return f"https://bsky.app/profile/{self._author_id}/post/{self._id}"

    @property
    def author(self) -> str:
        return self._author

    @cached_property
    def author_url(self) -> str:
        return f"https://bsky.app/profile/{self._author_id}"

    @property
    def text(self) -> str:
        return self._text

    @property
    def media(self) -> list[BskyMedia]:
        return self._media

    @property
    def sensitive(self) -> bool:
        return self._sensitive


class ProcessBsky:
    __slots__ = ('_url', '_id', '_bsky')

    def __init__(self, url: str):
        self._url: str = url

    async def __aenter__(self):
        bsky = await self._fetch_bsky()
        if not bsky['thread'].get('post'):
            raise ValueError(f"BSky post not found: {bsky}")
        self._bsky = bsky['thread']['post']
        return Bsky(
            id=self._id,
            author=self._bsky['author']['displayName'],
            author_id=self._bsky['author']['handle'],
            text=self._bsky['record']['text'],
            media=self._bsky_media,
            sensitive=self._sensitive
        )

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    async def _fetch_bsky(self) -> BskyInfo:
        match = bsky_url.match(self._url)
        if not match:
            raise ValueError(f"Invalid Bsky URL: {self._url}")
        auther_id, self._id = match.groups()
        return await NetClient.fetch_json(
            bsky_api_url,
            params={'uri': f'at://{auther_id}/app.bsky.feed.post/{self._id}', 'depth': 0}
        )

    @property
    def _bsky_media(self) -> list[BskyMedia]:
        if not self._bsky.get('embed'):
            return []
        match (embed := self._bsky['embed'])['$type']:
            case 'app.bsky.embed.images#view':
                embed: BskyEmbedImages
                return [
                    BskyMedia(
                        url=image['fullsize'],
                        thumb=image['thumb'],
                        media_type='image'
                    )
                    for image in embed['images']
                ]
            case 'app.bsky.embed.video#view':
                embed: BskyEmbedVideo
                return [
                    BskyMedia(
                        url=embed['playlist'],
                        thumb=embed['thumbnail'],
                        media_type='video'
                    )
                ]
            case 'app.bsky.embed.external#view':
                embed: BskyEmbedExternal
                return [
                    BskyMedia(
                        url=embed['external']['uri'],
                        thumb=embed['external']['thumb'],
                        media_type='external'
                    )
                ]
            case _:
                raise NotImplementedError(f"Unknown Bsky embed type: {embed['$type']}")

    @property
    def _sensitive(self) -> bool:
        return any(
            tag in self._bsky['labels']
            for tag in SENSITIVE_TAG
        )
