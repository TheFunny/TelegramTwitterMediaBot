# Repository Guidelines

## Project Overview

Telegram bot (teloxide) that turns post links from X/Twitter, Pixiv, and Bluesky into media messages (images, video, GIF) with the post's title, author, and tags. It supports batch media splitting, retry with persistence, inline queries, forward-channel rebinding with caption templates, and Pixiv ugoira→MP4 transcoding. README and user-facing strings are in Chinese. The project is a Rust port of a Python predecessor (see `queue.rs` comments referencing `utils/task_queue.py`).

Two-crate Cargo workspace (both v1.0.3, edition 2024, resolver 3):

- **`crates/x-media`** — library that fetches and normalizes media from the three sites. Pure, no Telegram knowledge.
- **`crates/xmedia-bot`** — the bot binary: teloxide dispatcher, SQLite-backed chat state, persistent task queue.

## Architecture & Data Flow

```
Telegram update → Dispatcher (polling or axum webhook) → dptree branches
  ├─ message    → commands (any chat) / URL links (private chat only)
  ├─ inline_query → InlineQueryResult Photo/Video/Mpeg4Gif
  └─ callback_query → "forward" (copy to channel) / "template|<name>" (apply caption template)
```

Message flow: `message_handler` extracts URLs (from `url`/`text_link` entities, text + caption, deduped) → `x_media::site::fetch(url)` → `Fetched` → builds a `Task` → `send::send_media_sequence` (media groups ≤ 9, caption on first item) or `send::send_animation`. On Telegram URL-fetch failure or size error (`send_batch_via_upload`): download via `x_media::site::download_media` to a temp file (≤ 10 MiB), sniff magic bytes (`sniff_ext`), upload via multipart; oversized items fall back to `fallback_url`. On failure: `enqueue_retry` persists resume-state `Task` into the SQLite queue → single worker leases (120 s lock TTL) → retry with exponential backoff (≤ 30 s, `MAX_RETRIES = 2`) → dead-letter → `notify_failure`. Success → `post_send_actions`: edit-before-forward prompt with inline buttons, or `copy_messages` to the bound forward channel.

The `x-media` library: `site::fetch(url)` dispatches (in order) twitter → bsky → pixiv via per-site regex `PATTERN` and returns `Ok(None)` for unmatched URLs. `Fetched { source_url, caption, title, media: Vec<Media>, sensitive, … }`; `caption_with(format)` substitutes `{url} {author} {author_url} {title} {tags}`.

## Key Directories

| Path | Purpose |
|---|---|
| `crates/x-media/src/` | Fetch library. `site/mod.rs` = dispatcher + `Fetched`/`FetchError`/`download_media`/`media_size`; `media.rs` = `Media` enum; `examples/fetch.rs` = end-to-end usage sample |
| `crates/x-media/src/site/<twitter\|pixiv\|bsky>/` | One directory per site: `mod.rs` (re-exports), `interface.rs` (PATTERN, `enabled()`, `fetch_from_url()`, site struct, `From<SiteStruct> for Fetched`), `model.rs` (serde DTOs). Pixiv adds `api.rs` (auth + transport) |
| `crates/xmedia-bot/src/main.rs` | Entry point: env/log init, queue worker start, pixiv validation, 300 s edit-expiry sweep, dptree handler tree, webhook vs polling dispatch |
| `crates/xmedia-bot/src/config.rs` | Manual env parsing into `Config` |
| `crates/xmedia-bot/src/handlers.rs` | `Command` enum (teloxide `BotCommands`), message/inline/callback handlers, URL extraction, global statics |
| `crates/xmedia-bot/src/state.rs` | `ChatStore`: parking_lot `Mutex<HashMap>` cache + SQLite write-through (`chat_state` table) |
| `crates/xmedia-bot/src/queue.rs` | `PersistentTaskQueue`: SQLite-backed single-worker queue (`tasks` table) |
| `crates/xmedia-bot/src/send.rs` | Media senders, upload fallback, error classification, queue task handlers |

## Development Commands

```bash
export TELOXIDE_TOKEN=<token>          # required; PIXIV_REFRESH_TOKEN optional (Pixiv disabled without it)
cargo run -p xmedia-bot                # run the bot (polling by default)
cargo run -p x-media --example fetch -- <url>   # test a link through the fetch library
cargo test --workspace                 # full test suite (no CI test step exists — run locally)
cargo build --release -p xmedia-bot    # release build (Dockerfile does this)
cargo clippy --workspace --all-targets # lint (Clippy is the configured IDE linter)
cargo fmt --check                      # formatting
```

Docker: `docker build -t tgxmb .` then `docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data tgxmb`. Runtime requires **ffmpeg** (built into the image).

## Code Conventions & Common Patterns

- **No anyhow/thiserror.** Errors are hand-rolled enums with manual `Display`/`source()`/`From` impls: `QueueError` (`Retryable { delay_seconds, payload }` / `Permanent`), `SendError` (Retryable/Permanent), `FetchError` (`Http`/`Json`/`Pixiv`/`NotFound`/`Blocked`), `PixivError`, `Classification`. New errors should follow this pattern.
- **Global state via `std::sync::LazyLock` statics**, not DI: `CONFIG`, `CHAT_STORE`, `TASK_QUEUE` in `handlers.rs`; shared reqwest `CLIENT` in `x-media/src/site/mod.rs`. `Bot` is passed/cloned into handlers; queue workers rebuild `Bot::from_env()`.
- **Async**: tokio multi-thread runtime (`#[tokio::main]` default). All rusqlite I/O inside `tokio::task::spawn_blocking`. Long loops use `tokio::select!` with `tokio::sync::{watch, Notify}` stop/wake channels. No streams.
- **Blocking sync primitives**: `parking_lot::Mutex` for hot caches, `tokio::sync::Mutex` for async-shared state (pixiv token cache), `AtomicBool` for feature gates.
- **Site adapter convention** (no trait, no enum dispatch — follow the existing convention): each site module exports `PATTERN: LazyLock<Regex>`, `enabled() -> bool`, `fetch_from_url(url) -> Result<Fetched, FetchError>`; `site/mod.rs` re-exports the site struct and `fetch_once` adds one guarded if-branch. Adding a site = new `site/<name>/{mod.rs,interface.rs,model.rs}` + one branch in `fetch_once`.
- **Serde**: per-site `model.rs` are pure `Deserialize` DTOs mirroring API JSON; site structs in `interface.rs` have private fields, a `caption()` builder, and `impl From<SiteStruct> for Fetched`. Persisted payloads use internally-tagged enums (`#[serde(tag = "kind")]` / `type`).
- **Naming**: module-per-concern, snake_case files, `CamelCase` types, `snake_case` fns. `//!` module docs and `///` docs on non-obvious logic (syndication token, ugoira encoding, `display_text_range`).
- **Retries**: only `x-media::site::fetch` retries (3 attempts, `1 << attempt` backoff, HTTP errors only). Queue retries are explicit `QueueError::Retryable` with computed delay (`retry_delay_seconds`).
- Logging via `log` macros (`pretty_env_logger`, level from `RUST_LOG`).

## Important Files

| File | Why it matters |
|---|---|
| `crates/xmedia-bot/src/main.rs` | Startup sequence, webhook vs polling, graceful shutdown (ctrlc → sweep stop → admin msg → queue stop) |
| `crates/xmedia-bot/src/handlers.rs` | `CHAT_STORE`/`TASK_QUEUE`/`CONFIG` singletons (open `data/task_queue.db` **relative to CWD**); command dispatch; URL extraction; retry enqueue |
| `crates/xmedia-bot/src/send.rs` | Constants `MAX_MEDIA_GROUP = 9`, `MAX_UPLOAD_BYTES = 10 MiB`; fallback chain; `classify_request_error` |
| `crates/x-media/src/site/mod.rs` | Dispatcher, `Fetched`/`FetchError`, shared `CLIENT`, `download_media` (adds `Referer: https://www.pixiv.net/` for `pximg.net` hotlink protection) |
| `crates/x-media/src/site/pixiv/api.rs` | OAuth token exchange (hardcoded app client id/secret), access-token cache, ugoira zip→MP4 via ffmpeg in `spawn_blocking` |
| `Dockerfile` | Multi-stage: cached dep layer via stub sources + `touch *.rs` mtime hack, static ffmpeg from ffmpeg.martin-riedl.de (`FFMPEG_URL` arg, `unzip -t` integrity check), `debian:bookworm-slim` runtime, entrypoint |
| `docker-entrypoint.sh` | Privilege drop: `useradd` with `LOCAL_USER_ID` (default 9001) + `setpriv` (no gosu on bookworm-slim) |
| `docker-compose.yml.example` | Deployment env reference (real `docker-compose.yml` is gitignored) |
| `.github/workflows/docker.yml` | CI: build+push to Docker Hub on tag `v*`/master; **no test step**; buildx gha cache (`cache-from`/`cache-to`, scope `tgxmb-build`, `mode=max`) so cargo deps + ffmpeg layers are restored across runs |
| `README.md` | Feature docs + command table (Chinese) |

## Runtime/Tooling Preferences

- **Rust, stable, edition 2024**, workspace resolver 3. No `rust-version`/MSRV pin, no `rust-toolchain.toml` — recent stable is assumed. No nightly features.
- Package manager: **Cargo** (workspace with path dep `x-media` ← `xmedia-bot`). No `[workspace.package]`/shared deps — each crate lists deps independently.
- **Two reqwest versions coexist in the lock** (0.12.28 via teloxide, 0.13.3 in x-media) — don't unify casually.
- Config is **environment-variable driven** (dotenv loads `.env`, gitignored; no `.env.example` exists). Key vars: `TELOXIDE_TOKEN` (required), `PIXIV_REFRESH_TOKEN`, `BOT_ADMIN` (comma-separated ids), `EDIT_MESSAGE_TTL_SECONDS` (default 86400), `WEBHOOK`/`WEBHOOK_URL`/`WEBHOOK_LISTEN`/`WEBHOOK_PORT`/`WEBHOOK_CERT`/`WEBHOOK_SECRET_TOKEN` (webhook mode requires cert + port, `.expect`ed), `RUST_LOG`, `TELOXIDE_PROXY`, `LOCAL_USER_ID` (entrypoint only).
- SQLite via `rusqlite` with `bundled` feature (no system libsqlite needed). DB file `data/task_queue.db` is CWD-relative — run from the workspace root, or `/app` in Docker. Mount `./data` and `./cert` volumes.
- `.gitattributes` enforces LF for `*.sh` (CRLF breaks shebangs in containers). `.gitignore`: `.env`, `data/`, `cert/`, `docker-compose.yml`, `/target`, `.idea/`.
- Docs are in Chinese; user-facing bot strings too. Keep that convention when editing captions/templates/docs.

## Testing & QA

- **~51 tests, all inline `#[cfg(test)] mod tests`** — no `tests/` integration directories. Framework: built-in Rust test + `#[tokio::test]` (dev-deps only in `x-media`: tokio macros/rt-multi-thread, dotenv).
- No mocking framework anywhere (no mockito/wiremock/mockall). Conventions: pure-function units (regex parsing, serde round-trips, chunking, retry math) tested synchronously; async tests use real dependencies — file-backed SQLite via `tempfile` (`queue.rs::new_queue()` helper), live network fetches.
- Live-network tests exist in `site/twitter/interface.rs` (3), `site/bsky/interface.rs` (2), `site/pixiv/interface.rs`/`api.rs` (env-gated on `PIXIV_REFRESH_TOKEN`/dotenv, skip by early return). Run the full suite with `cargo test --workspace`.
- Fixtures are inline `serde_json::json!` builder fns (`fixture()`, `thread_json()`, `illust_json()`), not files. The shared `CLIENT` sets `pool_max_idle_per_host(0)` under `#[cfg(test)]` to avoid cross-runtime `DispatchGone`.
- **CI runs no tests** — `.github/workflows/docker.yml` only builds/pushes the image; verification is a local responsibility.
- Untested and hard to test without a mock seam: `handlers.rs` (depends directly on teloxide `Bot`); `main.rs`, `config.rs`, `state.rs`; `media.rs`, `lib.rs`, all `model.rs`.
- No coverage tracking, no lint gate in CI.
