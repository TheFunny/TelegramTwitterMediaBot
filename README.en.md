# TelegramXMediaBot

A Telegram bot that turns post links from X / Twitter, Pixiv, Bluesky, Misskey (misskey.io), and Bilibili dynamics into media messages (images, video, GIF) with the post's title, author, and tags.

## Features

- Sending a link in a private chat fetches and sends the images, videos and GIFs automatically; oversized media is split into batches (10 items per group)
- Text-only posts report "no media"; unsupported links are silently ignored. Fetch failures name the reason (post gone / content withheld / source risk control / site not enabled)
- Long posts (text ≥ `CAPTION_QUOTE_TEXT_CHARS`, default 200) show **the text part** of their caption inside a collapsible blockquote, with the link and author line left outside it
- Inline queries (`@bot <link>`) — except Pixiv images and locally transcoded animations, which Telegram cannot fetch (no Referer) and would show broken, so they are skipped (such a query answers empty rather than spinning or re-fetching); a supported link posted in a group gets a one-line hint to use the private chat or inline mode (channels stay silent)
- `/start` explains the supported sites and how to use it; `/help` lists the commands plus argument syntax, the caption placeholders and the private-chat rule; the bot's profile description texts are set at startup
- `/settings` shows this chat's configuration (forward channel, edit-before-forward, per-site caption formats, saved templates); templates are added with `/set_template` and removed with `/remove_template`
- Bind a forward channel for automatic forwarding; edit the caption before forwarding and apply custom templates (the prompt carries Confirm / Skip buttons, states its expiry, and is marked expired in place once it lapses)
- Failed sends are retried automatically with persistence; the notice names which link failed, how long the retry waits, or the final cause
- The chat action stays on screen for the whole fetch, so long jobs (ugoira transcode, large uploads) do not look stalled
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

Docker deployment (`docker-compose.yml` in this repo is the orchestration; instance values live in the `.env` next to it, and compose substitutes every `${VAR}` from there):

```bash
cp .env.example .env   # fill in TELOXIDE_TOKEN and the rest; every line is commented
docker build -t tgxmb .
docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data tgxmb
# or use the bundled orchestration (nginx-proxy + acme-companion):
docker compose up -d
```

Environment variables: `TELOXIDE_TOKEN` (required), `PIXIV_REFRESH_TOKEN`, `BOT_ADMIN`, `EDIT_MESSAGE_TTL_SECONDS`, `LINK_CACHE_TTL_SECONDS`, `RUST_LOG`, `TELOXIDE_PROXY`, `WEBHOOK*`, `TWITTER_AUTH_TOKEN` (optional), `BILIBILI_COOKIE` (optional).

NSFW tweets: the public syndication endpoint does not return sensitive content. Setting `TWITTER_AUTH_TOKEN` (the `auth_token` cookie value of a logged-in x.com session) lets the bot fetch NSFW media in the logged-in state only when it hits a withheld tweet; without it the bot answers that the post's media is withheld and needs `TWITTER_AUTH_TOKEN`.

Bilibili dynamics are fetched anonymously by default (no login; the bot fetches bilibili's anonymous `buvid3`/`buvid4` device cookies itself to raise the success rate). If the server's egress IP gets hard-flagged by bilibili (persistent `risk control (-352)` log lines or HTTP 412), set `BILIBILI_COOKIE` (the whole cookie string from a logged-in browser, e.g. `SESSDATA=…; bili_jct=…`) to restore access. Only a dynamic's images and animations are sent; an attached video degrades to its cover image.

### Webhook deployment (needs a reverse proxy)

`docker-compose.yml` ships an [nginx-proxy](https://github.com/nginx-proxy/nginx-proxy) + [acme-companion](https://github.com/nginx-proxy/acme-companion) reverse-proxy orchestration. The committed file needs **no editing**: domain, tokens and admins are instance values and live in the `.env` beside it (compose reads and substitutes `${VAR}` at startup). Pick one deployment shape:

**With a domain**
1. Point a DNS A record at the server
2. In `.env` set `VIRTUAL_HOST` and `WEBHOOK_URL` to the domain; to have acme-companion issue the certificate, also uncomment the `ACME_HOST` line in `docker-compose.yml` and set `ACME_HOST` in `.env`
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
3. In `.env` set `VIRTUAL_HOST=<SERVER_IP>` and `WEBHOOK_URL=https://<SERVER_IP>/`; no `WEBHOOK_CERT` needed. Renewal is handled by the acme.sh daemon (`--days 3` = renew every 3 days, buffer against the 7-day validity), and a successful renewal HUP-notifies nginx-proxy to load the new certificate.

   Limitations: certificate validity ~7 days; only http-01/tls-alpn-01 validation (port 80 must be publicly reachable); no DNS-01, private IPs or IP ranges; at most 5 certificates per 168 hours for the same IP set. It is recommended to trial-issue with `--server letsencrypt_test` first, then switch to the production server.

Telegram only accepts ports 443/80/88/8443.

<details>
<summary>Environment variables</summary>

| Variable | Description |
|---|---|
| `TELOXIDE_TOKEN` | Bot token (required) |
| `PIXIV_REFRESH_TOKEN` | Pixiv refresh token; Pixiv is disabled without it (a pixiv link then gets an explicit "site not enabled" reply instead of silence) |
| `TWITTER_AUTH_TOKEN` | Optional; the `auth_token` cookie of a logged-in x.com session, used only to fetch NSFW tweets' media |
| `BILIBILI_COOKIE` | Optional bilibili cookie string (`SESSDATA=…; bili_jct=…`); only needed when the egress IP stays risk-controlled (device cookies are fetched automatically) |
| `BOT_ADMIN` | Admin chat IDs, comma-separated; receives start/stop notifications |
| `EDIT_MESSAGE_TTL_SECONDS` | Edit-before-forward record expiry in seconds, default 86400; once lapsed the prompt is rewritten in place to "expired — nothing was forwarded" (no extra message) |
| `LINK_CACHE_TTL_SECONDS` | Link-result cache expiry in seconds, default 604800 (7 days) |
| `CAPTION_QUOTE_TEXT_CHARS` | **The text part** of the caption (the joined `{title}` + `{content}`) is wrapped in a collapsible blockquote once it reaches this many characters, default 200; `0` disables |
| `DATA_DIR` | Data directory (where the SQLite `task_queue.db` lives), default `data` (relative to the working directory, created automatically) |
| `RUST_LOG` | Log level, default `info,hyper_util=warn,reqwest=warn` (an unset variable no longer silences the log). Recipes: `info,xmedia_bot=debug,x_media=debug` (app detail, no dependency noise) / `debug,hyper_util=off` (everything) / `trace` (also prints full links and message text — **user data**) |
| `TELOXIDE_PROXY` | HTTP proxy (e.g. `http://127.0.0.1:10808`); applies to both the Telegram Bot API and site fetches — required on restricted networks (e.g. behind the GFW). **Never leave it blank** (`TELOXIDE_PROXY=`) — teloxide panics on a value it cannot parse; omit the line when unused. `docker-compose.yml` deliberately does not pass it to the container (a `127.0.0.1` proxy there is the container itself): add the line and use `host.docker.internal:<port>` when a deployment needs one |
| `LOCAL_USER_ID` | UID the container runs as, default 9001 |
| `VIRTUAL_HOST` | Public domain or IP; nginx-proxy routes by this (set it in `.env`, which compose reads) |
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
| `/edit_before_forward` | Toggle "edit before forward": when enabled, the bot posts a prompt after forwarding; replying to it edits the first forwarded message's caption (or tapping a template button applies one), then `↩️ Confirm` forwards and `🛑 Skip` drops this forward; the prompt states its expiry and is marked expired in place when it lapses (nothing is forwarded) |
| `/set_template <name>` | Reply to a message containing `[]` to save it as a named template; `[]` is replaced by the original post link when forwarding (used with "edit before forward") |
| `/remove_template <name>` | Remove a template (names are listed by `/settings`; the prompt's keyboard shows at most 60) |
| `/settings` | Show this chat's configuration: forward channel, edit-before-forward, per-site caption formats, saved templates |
| `/set_format <site> <format>` | Customize the caption format for one site. Sites: `twitter` / `bsky` / `pixiv` / `misskey` / `bilibili`. Placeholders: `{url}` `{author}` `{author_url}` `{title}` `{content}` `{tags}`; unknown placeholders are rejected with the list of valid ones, and `-` restores the site's built-in format (preview with `/debug <link>`) |
| `/clear_cache [link]` | Clear the link cache (admin only); with a link only that entry, otherwise everything |
| `/bot_dict` | Show the current chat state (debugging; admin only) |
| `/test <link>` | Parse a link and send its media; no channel forward, no edit-before-forward prompt (send only) |
| `/debug <link>` | Debug: parse a link and report the parse result only (site, title, author, tags, media list) — no media is sent |

Link processing works only in private chats; commands work in any chat. A supported link posted in a group gets a one-line hint to use the private chat or inline mode; channels stay silent.

## Notes

- State is persisted in `data/task_queue.db`; compose deployments use the bind mount `./data` (keep it a directory for easy backups)
- The runtime needs ffmpeg (built into the Docker image)
- Tests: `cargo test --workspace`
