from __future__ import annotations

from common import PIXIV_REFRESH_TOKEN
from .pixiv import TelegramPixiv
from .regex import pixiv_url, x_url
from .tweet import TelegramTweet


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
