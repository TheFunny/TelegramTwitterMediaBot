from __future__ import annotations

from common import x_url_regex
from .net import NetClient
from .tweet import TelegramTweet


class Telegram(NetClient):
    def __init__(self, url: str):
        self._url = url

    async def __aenter__(self):
        if x_url_regex.match(self._url):
            async with TelegramTweet(self._url) as tweet:
                return tweet

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        pass
