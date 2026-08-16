# TelegramXMediaBot

A Telegram bot that turns post links from X / Twitter, Pixiv, and Bluesky into media messages (images, video, GIF) with the post's title, author, and tags.

## Features

- Sending a link in a private chat fetches and sends the images, videos and GIFs automatically; oversized media is split into batches
- Text-only posts report "no media"; unsupported links are silently ignored
- Inline queries (`@bot <link>`)
- Bind a forward channel for automatic forwarding; edit the caption before forwarding and apply custom templates
- Failed sends are retried automatically with persistence; the user is notified after retries are exhausted
- Pixiv ugoira animations are transcoded to MP4; Bluesky videos are remuxed (HLS stream → MP4)
- Photos exceeding Telegram's size/dimension limits are compressed automatically (original format kept, JPEG fallback only when needed)
- Link-result cache: after a successful send the Telegram file ids and caption fields are cached locally, so a repeated link is re-sent from local state — no source-site request, no media file stored (expiry controlled by `LINK_CACHE_TTL_SECONDS`, default 7 days)

## Quick start

```bash
# Required: BotFather token; optional: PIXIV_REFRESH_TOKEN (Pixiv is disabled without it)
export TELOXIDE_TOKEN=<token>
export PIXIV_REFRESH_TOKEN=<token>

cargo run -p xmedia-bot
```

Docker deployment (see `docker-compose.yml.example`):

```bash
docker build -t tgxmb .
docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data tgxmb
```

Environment variables: `TELOXIDE_TOKEN` (required), `PIXIV_REFRESH_TOKEN`, `BOT_ADMIN`, `EDIT_MESSAGE_TTL_SECONDS`, `LINK_CACHE_TTL_SECONDS`, `RUST_LOG`, `TELOXIDE_PROXY`, `WEBHOOK*`, `TWITTER_AUTH_TOKEN` (optional).

NSFW tweets: the public syndication endpoint does not return sensitive content. Setting `TWITTER_AUTH_TOKEN` (the `auth_token` cookie value of a logged-in x.com session) lets the bot fetch NSFW media in the logged-in state only when it hits a withheld tweet; without it, the bot reports no media.

### Webhook deployment (needs a reverse proxy)

`docker-compose.yml.example` ships an [nginx-proxy](https://github.com/nginx-proxy/nginx-proxy) + [acme-companion](https://github.com/nginx-proxy/acme-companion) reverse-proxy orchestration. Pick one deployment shape:

**With a domain**
1. Point a DNS A record at the server
2. In compose set `VIRTUAL_HOST` and `WEBHOOK_URL` to the domain, and uncomment `ACME_HOST` (set it to the domain)
3. acme-companion issues and renews certificates automatically — nothing manual

**IP only**
Let's Encrypt can issue certificates for public IPs (available since 2026, validity ~7 days, requires the `shortlived` profile). Use [acme.sh](https://github.com/acmesh-official/acme.sh) to issue and renew automatically, no manual certificates:

1. Add an acme-ip service to compose (issue + daily auto-renewal check):
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
2. First issuance (replace `<SERVER_IP>` with the server's public IP; IPv6 works too, repeat `-d` for more):
   ```bash
   docker compose exec acme-ip acme.sh --issue --server letsencrypt \
     -d <SERVER_IP> --cert-profile shortlived --days 3 \
     --webroot /usr/share/nginx/html \
     --install-cert --cert-file /acme.sh/<SERVER_IP>.crt \
     --key-file /acme.sh/<SERVER_IP>.key \
     --reloadcmd "curl --unix-socket /var/run/docker.sock -X POST http://localhost/containers/nginx-proxy/kill?signal=HUP"
   ```
3. In compose set `VIRTUAL_HOST: '<SERVER_IP>'` and `WEBHOOK_URL: 'https://<SERVER_IP>/'`; no `WEBHOOK_CERT` needed. Renewal is handled by the acme.sh daemon (`--days 3` = renew every 3 days, buffer against the 7-day validity), and a successful renewal HUP-notifies nginx-proxy to load the new certificate.

   Limitations: certificate validity ~7 days; only http-01/tls-alpn-01 validation (port 80 must be publicly reachable); no DNS-01, private IPs or IP ranges; at most 5 certificates per 168 hours for the same IP set. It is recommended to trial-issue with `--server letsencrypt_test` first, then switch to the production server.

Telegram only accepts ports 443/80/88/8443.

<details>
<summary>Environment variables</summary>

| Variable | Description |
|---|---|
| `TELOXIDE_TOKEN` | Bot token (required) |
| `PIXIV_REFRESH_TOKEN` | Pixiv refresh token; Pixiv is disabled without it |
| `BOT_ADMIN` | Admin chat IDs, comma-separated; receives start/stop notifications |
| `EDIT_MESSAGE_TTL_SECONDS` | Edit-before-forward record expiry in seconds, default 86400 |
| `LINK_CACHE_TTL_SECONDS` | Link-result cache expiry in seconds, default 604800 (7 days) |
| `RUST_LOG` | Log level |
| `TELOXIDE_PROXY` | HTTP proxy (e.g. `http://127.0.0.1:10808`); applies to both the Telegram Bot API and site fetches — required on restricted networks (e.g. behind the GFW) |
| `LOCAL_USER_ID` | UID the container runs as, default 9001 |
| `VIRTUAL_HOST` | Public domain or IP; nginx-proxy routes by this |
| `VIRTUAL_PORT` | Port the bot listens on inside the container; nginx-proxy's forwarding target |
| `ACME_HOST` | Domain deployment: when set to the domain, acme-companion issues/renews certificates automatically |
| `DEFAULT_HOST` | nginx-proxy routes requests with unknown Host headers to this vhost (needed for IP access) |
| `DEFAULT_EMAIL` | acme-companion certificate notification email |
| `WEBHOOK` | `true` enables webhook mode (polling by default) |
| `WEBHOOK_LISTEN` / `WEBHOOK_PORT` | Listen address/port inside the bot container |
| `WEBHOOK_URL` | Public HTTPS URL (`https://domain/` or `https://IP/`) |
| `WEBHOOK_CERT` | Optional; self-signed certificate path, only used for Telegram-side validation (TLS is terminated by the reverse proxy) |
| `WEBHOOK_SECRET_TOKEN` | Update validation token (`X-Telegram-Bot-Api-Secret-Token`) |

</details>

## Commands

| Command | Description |
|---|---|
| `/start` | Welcome message |
| `/help` | List all commands and usage (this command table) |
| `/set_forward_channel <channel>` | Set the forward channel: `@channel` or channel ID; media messages are forwarded to it automatically afterwards |
| `/remove_forward_channel` | Remove the forward channel |
| `/edit_before_forward` | Toggle "edit before forward": when enabled, the bot posts a prompt after forwarding; replying to it edits the first forwarded message's caption (or taps a template button to apply one) |
| `/set_template <name>` | Reply to a message containing `[]` to save it as a named template; `[]` is replaced by the original post link when forwarding (used with "edit before forward") |
| `/set_format <site> <format>` | Customize the caption format for one site. Sites: `twitter` / `bsky` / `pixiv`. Placeholders: `{url}` `{author}` `{author_url}` `{title}` `{tags}` |
| `/clear_cache [link]` | Clear the link cache (admin only); with a link only that entry, otherwise everything |
| `/bot_dict` | Show the current chat state (debugging) |
| `/test <link>` | Debug: parse a link and report the parse result only (site, title, author, tags, media list) — no media is sent |

Link processing works only in private chats; commands work in any chat.

## Notes

- State is persisted in `data/task_queue.db`; compose deployments use the bind mount `./data` (keep it a directory for easy backups)
- The runtime needs ffmpeg (built into the Docker image)
- Tests: `cargo test --workspace`
