from __future__ import annotations

from typing import Literal, TYPE_CHECKING

from async_pixiv import PixivClient
from async_pixiv.error import ApiError

if TYPE_CHECKING:
    from async_pixiv.model.illust import Illust


class PixivMedia:
    __slots__ = ('_url', '_thumb')

    def __init__(self, url: str, thumb: str):
        self._url: str = url
        self._thumb: str = thumb

    def __str__(self):
        return f"PixivMedia(url={self.url}, thumb={self.thumb}, large={self.large})"

    @property
    def url(self) -> str:
        return self._url

    @property
    def thumb(self) -> str:
        return self._thumb

    @property
    def large(self) -> str:
        url = self._url.replace("img-original", "img-master").removesuffix(".jpg").removesuffix(".png")
        return url + "_master1200.jpg"


class Pixiv:
    __slots__ = ('_illust',)

    def __init__(self, illust: Illust):
        self._illust: Illust = illust

    @property
    def url(self) -> str:
        return str(self._illust.link).rstrip('/')

    @property
    def type(self) -> Literal["illust", "manga", "ugoira"]:
        return self._illust.type.value

    @property
    def title(self) -> str:
        return self._illust.title

    @property
    def author(self) -> str:
        return self._illust.user.name

    @property
    def author_url(self) -> str:
        return str(self._illust.user.link).rstrip('/')

    @property
    def description(self) -> str:
        return self._illust.caption

    @property
    def tags(self) -> list[str]:
        return [tag.name for tag in self._illust.tags]

    @property
    def is_multiple_pages(self) -> bool:
        return self._illust.page_count > 1

    @property
    def is_nsfw(self) -> bool:
        return self._illust.is_nsfw

    @property
    def is_ai(self) -> bool:
        return self._illust.ai_type.value == 2

    @property
    def images(self) -> list[PixivMedia]:
        if self.is_multiple_pages:
            return [
                PixivMedia(
                    url=page.image_urls.original,
                    thumb=page.image_urls.medium
                )
                for page in self._illust.meta_pages
            ]
        else:
            return [
                PixivMedia(  # should observe if image_url.original is always None for single page
                    url=self._illust.image_urls.original or self._illust.meta_single_page.original,
                    thumb=self._illust.image_urls.medium
                )
            ]


class _ProcessPixiv:
    _client: PixivClient

    @classmethod
    async def init_client(cls, token: str) -> None:
        cls._client = PixivClient()
        await cls._client.login_with_token(token)
        cls._token = token

    @classmethod
    async def close_client(cls) -> None:
        await cls._client.close()

    @classmethod
    async def refresh_token(cls):
        await cls._client.login_with_token(cls._token)


class ProcessPixiv(_ProcessPixiv):
    __slots__ = ('_url', '_illust')

    def __init__(self, url: str):
        self._url: str = url

    async def __aenter__(self):
        self._illust = await self._fetch_illust()
        return Pixiv(self._illust)

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass

    async def _fetch_illust(self) -> Illust:
        illust_id = self._parse_illust_id()
        try:
            return (await self._client.ILLUST.detail(illust_id)).illust
        except ApiError:
            await self.refresh_token()
            return (await self._client.ILLUST.detail(illust_id)).illust  # TODO use retry here

    def _parse_illust_id(self) -> int:
        return int(self._url.split("/")[-1])
