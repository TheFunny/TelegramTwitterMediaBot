# ---------- build stage ----------
# rust:1-bookworm (full, not slim) ships the C toolchain needed by
# rusqlite's bundled SQLite, plus wget/unzip for the ffmpeg download.
FROM rust:1-bookworm AS builder

ARG APP_NAME=telegram-twitter-media-bot
# Prebuilt static ffmpeg (glibc-linked, includes libx264) for ugoira MP4
# encoding. Served from https://ffmpeg.martin-riedl.de (Cloudflare CDN,
# built on Debian 12 — glibc-compatible with the bookworm-slim runtime).
# johnvansickle.com throttles datacenter IPs and served garbage from GitHub
# runners.
#
# Pinned to one release build instead of `/redirect/latest/`: the floating
# URL changes under every build and ships no sha256 sidecar, while this pair
# (zip + the sha256 the mirror publishes beside it, `<url>.sha256`) is
# verified on every run. Bump both together — the site lists the current
# ids, e.g. https://ffmpeg.martin-riedl.de. Swap `amd64` for `arm64` when
# building arm64 images (the workflow builds amd64 only — see docker.yml).
ARG FFMPEG_URL=https://ffmpeg.martin-riedl.de/download/linux/amd64/1789931100_9.0.2/ffmpeg.zip
# sha256 of that zip, checked unconditionally: an FFMPEG_URL override must
# pair with the new zip's sha256 or the build fails here, so an unverifiable
# binary never reaches the image.
ARG FFMPEG_SHA256=fa8ecf4abbd290d98f7d188b8649cc6b391ae209a98452be955a15aab1909d7f

WORKDIR /build

# 1. Static ffmpeg first: only the two ARGs above invalidate this layer, so a
#    manifest or source edit never re-downloads it. The zip contains a single
#    `ffmpeg` binary at the root. `unzip -t` verifies the archive before
#    extraction so a bad download fails loudly here instead of a cryptic
#    later error.
RUN wget -q -O /tmp/ffmpeg.zip "$FFMPEG_URL" \
    && echo "$FFMPEG_SHA256  /tmp/ffmpeg.zip" | sha256sum -c - \
    && unzip -tq /tmp/ffmpeg.zip \
    && unzip -q /tmp/ffmpeg.zip -d /usr/local/bin \
    && chmod +x /usr/local/bin/ffmpeg \
    && rm /tmp/ffmpeg.zip \
    && /usr/local/bin/ffmpeg -version >/dev/null

# 2. Rust dependencies next: only the manifests plus stub sources, so the
#    expensive dependency fetch + compile lives in a layer invalidated only by
#    manifest/lock changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/x-media/Cargo.toml crates/x-media/Cargo.toml
COPY crates/xmedia-bot/Cargo.toml crates/xmedia-bot/Cargo.toml
RUN mkdir -p crates/x-media/src crates/xmedia-bot/src \
    && printf 'fn main() {}\n' > crates/xmedia-bot/src/main.rs \
    && : > crates/x-media/src/lib.rs \
    && cargo build --release --locked -p xmedia-bot

# 3. Real sources last: only our crates recompile on source changes. Cargo's
#    freshness check is mtime-based; the COPY'd host files usually predate the
#    stub build, so cargo would consider the stub up to date and never
#    compile the real sources. `touch` makes every .rs newer than the stub
#    artifacts, forcing a rebuild of just the two crates while the compiled
#    dependency layer stays cached. (`cargo clean -p` does NOT work here — it
#    removes 0 files and the stub binary silently ships.)
COPY crates/ ./crates/
RUN find crates -type f -name '*.rs' -exec touch {} + \
    && cargo build --release --locked -p xmedia-bot

# ---------- runtime stage ----------
FROM debian:bookworm-slim

# ARG scope is per-stage: re-declare for the label below.
ARG APP_NAME=telegram-twitter-media-bot

LABEL maintainer="admin@yoursfunny.top"
LABEL org.opencontainers.image.title="${APP_NAME}"

# Everything is copied in — no apt in the runtime stage. Privilege dropping is
# done by docker-entrypoint.sh with setpriv (util-linux, already in
# bookworm-slim), so no gosu needed. TLS is rustls (webpki-roots baked in,
# see Cargo.toml feature `rustls`/`rustls-tls`), so no system CA bundle or
# libssl are needed; the static ffmpeg only processes local files (all
# downloads go through reqwest).
COPY --from=builder /usr/local/bin/ffmpeg /usr/local/bin/ffmpeg

WORKDIR /app
COPY --from=builder /build/target/release/xmedia-bot /usr/local/bin/xmedia-bot
COPY docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod a+x /app/docker-entrypoint.sh

# State lives in /app/data (SQLite task queue + chat state); mount a volume
# there to keep it across restarts.
ENTRYPOINT ["/app/docker-entrypoint.sh"]
CMD ["xmedia-bot"]
