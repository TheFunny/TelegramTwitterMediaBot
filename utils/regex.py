import re

x_url = re.compile(r"^(?:https?://)?(?:www\.|mobile\.)?(?:x|twitter|fixvx|vxtwitter)\.com/(.+)/status/(\d+)")
x_media_url = re.compile(r"^(?:https?://)?(pbs|video)\.twimg\.com/(.*)")
x_tco_url = re.compile(r"(?:https?://)?t\.co/.+$", re.M)
message_url = re.compile(r"\[.+]", re.S)

pixiv_url = re.compile(r"^(?:https?://)?(?:www\.)?pixiv\.net/(?:en/)?(?:artworks/|i/)(\d+)")
