from __future__ import annotations

from .net import NetClient
from .regex import x_url
from .tweet import TelegramTweet


class Telegram(NetClient):
    def __init__(self, url: str):
        self._url = url

    async def __aenter__(self):
        if x_url.match(self._url):
            async with TelegramTweet(self._url) as tweet:
                return tweet

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass
