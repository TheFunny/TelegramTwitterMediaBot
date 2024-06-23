from __future__ import annotations

from .net import NetClient
from .pixiv import TelegramPixiv
from .regex import pixiv_url, x_url
from .tweet import TelegramTweet


class Telegram(NetClient):
    def __init__(self, url: str):
        self._url = url

    async def __aenter__(self):
        if x_url.match(self._url):
            async with TelegramTweet(self._url) as tweet:
                return tweet
        elif pixiv_url.match(self._url):
            async with TelegramPixiv(self._url) as pixiv:
                return pixiv

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass
