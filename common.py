import os

try:
    import uvloop
    import asyncio

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
except ImportError:
    uvloop = None

BOT_TOKEN = os.getenv("BOT_TOKEN")
ADMIN = [int(i) for i in os.getenv("BOT_ADMIN", "").split(",") if i]

PIXIV_REFRESH_TOKEN = os.getenv("PIXIV_REFRESH_TOKEN")

WEBHOOK = os.getenv("WEBHOOK", False)
if WEBHOOK:
    WEBHOOK_LISTEN = os.getenv("WEBHOOK_LISTEN", "0.0.0.0")
    WEBHOOK_PORT = int(os.getenv("WEBHOOK_PORT", 8443))
    WEBHOOK_URL = os.getenv("WEBHOOK_URL")
    WEBHOOK_KEY = os.getenv("WEBHOOK_KEY", "cert/private.key")
    WEBHOOK_CERT = os.getenv("WEBHOOK_CERT", "cert/cert.pem")
    WEBHOOK_SECRET_TOKEN = os.getenv("WEBHOOK_SECRET_TOKEN")
