import logging
import os
import re

BOT_TOKEN = os.getenv("BOT_TOKEN")
ADMIN = os.getenv("BOT_ADMIN").split(",")

WEBHOOK = os.getenv("WEBHOOK", False)
if WEBHOOK:
    WEBHOOK_LISTEN = os.getenv("WEBHOOK_LISTEN", "0.0.0.0")
    WEBHOOK_PORT = int(os.getenv("WEBHOOK_PORT", 8443))
    WEBHOOK_URL = os.getenv("WEBHOOK_URL")
    WEBHOOK_KEY = os.getenv("WEBHOOK_KEY", "cert/private.key")
    WEBHOOK_CERT = os.getenv("WEBHOOK_CERT", "cert/cert.pem")
    WEBHOOK_SECRET_TOKEN = os.getenv("WEBHOOK_SECRET_TOKEN")

x_url_regex = re.compile(r"^(?:https?://)(?:www\.|mobile\.|)(?:x|twitter)\.com/(.+)/status/(\d+)")
x_media_regex = re.compile(r"^(?:https?://)(pbs|video)\.twimg\.com/(.*)")
x_tco_regex = re.compile(r"(?:https?://)t\.co/.+$", re.M)
message_url_regex = re.compile(r"\{\w+\}", re.M)

logging.basicConfig(
    level=os.getenv("LOG_LEVEL", "WARNING"),
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger(__name__)
