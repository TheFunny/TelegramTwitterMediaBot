# TelegramXMediaBot

[中文](README.md)

A Telegram bot that converts post links from X / Twitter, Pixiv, Bluesky, Misskey (`misskey.io`), and Bilibili dynamics into media messages with title, author, and tags.

## Features

- **Multi-site media fetching**: Images, videos, and GIFs; automatically transcodes Pixiv ugoira and Bluesky HLS videos to MP4; sends Bilibili dynamic images/GIFs (cover image for embedded videos)
- **Smart formatting & handling**: Splits media into batches of 10 and auto-compresses oversized photos; delivers text-only posts as HTML messages and collapses long post bodies into expandable blockquotes
- **Inline mode & caching**: Supports `@bot <link>` inline queries (skips uncached media requiring a Referer or local transcoding); caches Telegram `file_id`s locally to re-send repeat links without re-fetching
- **Forwarding & templates**: Bind a forward channel, edit captions before forwarding with reusable templates, and customize per-site caption formats
- **Persistent retries**: Automatically retries failed sends with persistence and notifies on permanent failure

## Quick Start

### Docker (Recommended)

```bash
cp .env.example .env   # Fill in TELOXIDE_TOKEN and options (documented inline)

# Run prebuilt image directly (polling mode)
docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data \
  yoursfunny/telegram-twitter-media-bot:latest

# Or use the bundled compose stack (nginx-proxy + acme-companion)
docker compose up -d
```

### Local Run

```bash
export TELOXIDE_TOKEN=<token>          # Required
export PIXIV_REFRESH_TOKEN=<token>     # Optional; Pixiv is disabled without it
cargo run -p xmedia-bot
```

> Local runs require `ffmpeg` in `PATH` (bundled in the Docker image).

### Webhook Deployment

`docker-compose.yml` bundles `nginx-proxy` + `acme-companion` and reads all instance values from `.env`. Telegram only accepts ports `443 / 80 / 88 / 8443`, and `WEBHOOK_SECRET_TOKEN` is required when `WEBHOOK=true`.

- **With a domain**: Point a DNS A record to your server, set `VIRTUAL_HOST`, `WEBHOOK_URL`, and `ACME_HOST` in `.env`, and uncomment `ACME_HOST` in `docker-compose.yml` for automatic certificate issuance and renewal.

<details>
<summary>Public IP only (Let's Encrypt shortlived + acme.sh)</summary>

Requires public port 80 (`http-01` validation); certificates last ~7 days and renew automatically via `acme.sh`:

1. Add an `acme-ip` service to `docker-compose.yml`:
   ```yaml
   acme-ip:
     image: neilpang/acme.sh
     container_name: acme-ip
     command: daemon
     restart: always
     volumes:
       - certs:/acme.sh
       - html:/usr/share/nginx/html
       - /var/run/docker.sock:/var/run/docker.sock:ro
     networks: [proxy]
   ```
2. Issue the initial certificate (replace `<SERVER_IP>`):
   ```bash
   docker compose exec acme-ip acme.sh --issue --server letsencrypt \
     -d <SERVER_IP> --cert-profile shortlived --days 3 \
     --webroot /usr/share/nginx/html \
     --install-cert --cert-file /acme.sh/<SERVER_IP>.crt \
     --key-file /acme.sh/<SERVER_IP>.key \
     --reloadcmd "curl --unix-socket /var/run/docker.sock -X POST http://localhost/containers/nginx-proxy/kill?signal=HUP"
   ```
3. Set `VIRTUAL_HOST=<SERVER_IP>`, `DEFAULT_HOST=<SERVER_IP>`, and `WEBHOOK_URL=https://<SERVER_IP>/` in `.env` (no `WEBHOOK_CERT` needed).

</details>

<details>
<summary>Environment variables</summary>

| Variable | Default | Description |
|---|---|---|
| `TELOXIDE_TOKEN` | — | Bot token (required) |
| `PIXIV_REFRESH_TOKEN` | — | Pixiv refresh token; Pixiv is disabled when unset |
| `TWITTER_AUTH_TOKEN` | — | Logged-in x.com `auth_token` cookie, used only for NSFW tweets |
| `BILIBILI_COOKIE` | — | Full Bilibili cookie string (`SESSDATA=…; bili_jct=…`), needed only if egress IP stays risk-controlled (`-352`/412) |
| `BOT_ADMIN` | — | Comma-separated admin chat IDs for start/stop notices and admin commands |
| `EDIT_MESSAGE_TTL_SECONDS` | `86400` | Edit-before-forward prompt expiry in seconds |
| `LINK_CACHE_TTL_SECONDS` | `604800` | Link cache expiry in seconds (7 days) |
| `CAPTION_QUOTE_TEXT_CHARS` | `200` | Collapse post body into a blockquote at or above this character count (`0` disables) |
| `DATA_DIR` | `data` | Directory for the SQLite database `task_queue.db` |
| `RUST_LOG` | `info,hyper_util=warn,reqwest=warn` | Log filter (e.g. `info,xmedia_bot=debug,x_media=debug` for debugging) |
| `TELOXIDE_PROXY` | — | HTTP proxy (e.g. `http://127.0.0.1:10808`); **never leave blank** (omit when unused); in Docker add it to `docker-compose.yml` using `host.docker.internal` |
| `LOCAL_USER_ID` | `9001` | UID inside the container |
| `WEBHOOK` | `false` | Set `true` to enable webhook mode (polling by default) |
| `WEBHOOK_LISTEN` / `WEBHOOK_PORT` | `0.0.0.0` / `8443` | Listen address and port inside the container |
| `WEBHOOK_URL` | — | Public HTTPS URL (`https://domain/` or `https://IP/`) |
| `WEBHOOK_SECRET_TOKEN` | — | Webhook verification token (required when `WEBHOOK=true`) |
| `WEBHOOK_CERT` | — | Self-signed certificate path (Telegram-side validation only) |
| `VIRTUAL_HOST` / `VIRTUAL_PORT` | — / `8443` | `nginx-proxy` public domain/IP and container target port |
| `ACME_HOST` / `DEFAULT_EMAIL` | — | `acme-companion` certificate domain and notification email |
| `DEFAULT_HOST` | — | Default `nginx-proxy` vhost (needed for IP-only access) |

</details>

## Commands

| Command | Description |
|---|---|
| `/start` | Show introduction and supported sites |
| `/help` | List all commands and usage |
| `/set_forward_channel <channel>` | Bind a forward channel (`@channel` or channel ID) |
| `/remove_forward_channel` | Remove the forward channel |
| `/edit_before_forward` | Toggle edit-before-forward: reply to the prompt or tap a template button to edit the caption, then `Confirm` to forward or `Skip` to cancel |
| `/set_template <name>` | Reply to a message containing `[]` to save a template (`[]` is replaced with the post URL) |
| `/remove_template <name>` | Delete a saved template |
| `/settings` | Show this chat's forwarding, format, and template settings |
| `/set_format <site> <format>` | Customize caption format for a site (`twitter`/`bsky`/`pixiv`/`misskey`/`bilibili`; placeholders: `{url}` `{author}` `{author_url}` `{title}` `{content}` `{tags}`; `-` resets to default) |
| `/test <link>` | Send media without forwarding or opening an edit prompt |
| `/debug <link>` | Parse a link and preview metadata and caption without sending media |
| `/clear_cache [link]` | Clear link cache (admin only; omit link to clear all) |
| `/bot_dict` | Dump raw chat state for debugging (admin only) |

> Link processing works in private chats only (groups get a hint to use private chat or inline mode; channels stay silent). Commands work in any chat.

## Notes

- State is persisted in `data/task_queue.db` (bind-mount `./data:/app/data` in Docker)
- Tests: `cargo test --workspace`
