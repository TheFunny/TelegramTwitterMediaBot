from __future__ import annotations

from httpx import AsyncClient


def create_client() -> AsyncClient:
    return AsyncClient(http2=True)


async def close_client(_client: AsyncClient) -> None:
    return await _client.aclose()


async def fetch_json(_client: AsyncClient, url: str, params: dict = None) -> dict:
    response = await _client.get(url, params=params)
    assert response.is_success, f"Failed to fetch {url}, status code {response.status_code}"
    return response.json()


class NetClient:
    _httpx_client: AsyncClient

    @classmethod
    def init_client(cls) -> None:
        cls._httpx_client = create_client()

    @classmethod
    async def close_client(cls) -> None:
        await close_client(cls._httpx_client)

    @classmethod
    def get_client(cls) -> AsyncClient:
        return cls._httpx_client

    @classmethod
    async def fetch_json(cls, url: str, params: dict = None) -> dict:
        return await fetch_json(cls._httpx_client, url, params)
