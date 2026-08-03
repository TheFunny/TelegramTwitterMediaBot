# ---------- build stage ----------
# rust:1-bookworm (full, not slim) ships the C toolchain needed by
# rusqlite's bundled SQLite, plus wget/xz for the ffmpeg download.
FROM rust:1-bookworm AS builder

ARG APP_NAME=telegram-twitter-media-bot
# Statically compiled ffmpeg (ugoira MP4 encoding). amd64 by default; override
# for other platforms or pin a different johnvansickle build.
ARG FFMPEG_URL=https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-amd64-static.tar.xz

WORKDIR /build

# 1. Rust dependencies first: only the manifests plus stub sources, so the
#    expensive dependency fetch + compile lives in a layer invalidated only by
#    manifest/lock changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/x-media/Cargo.toml crates/x-media/Cargo.toml
COPY crates/xmedia-bot/Cargo.toml crates/xmedia-bot/Cargo.toml
RUN mkdir -p crates/x-media/src crates/xmedia-bot/src \
    && printf 'fn main() {}\n' > crates/xmedia-bot/src/main.rs \
    && : > crates/x-media/src/lib.rs \
    && cargo build --release -p xmedia-bot

# 2. Static ffmpeg next (cached unless FFMPEG_URL changes), so source edits
#    never re-download it. The johnvansickle tarball has a
#    `{build}/ffmpeg` layout, so strip one path component.
RUN wget -q -O /tmp/ffmpeg.tar.xz "$FFMPEG_URL" \
    && tar -xJf /tmp/ffmpeg.tar.xz -C /usr/local/bin --strip-components=1 --wildcards '*/ffmpeg' \
    && rm /tmp/ffmpeg.tar.xz \
    && /usr/local/bin/ffmpeg -version >/dev/null

# 3. Real sources last: only our crates recompile on source changes. The
#    COPY preserves host mtimes, which predate the stub artifacts from step 1;
#    cargo's mtime-based freshness check would otherwise treat the stub build
#    as up-to-date and never compile the real sources. `touch` forces cargo to
#    see the real files as newer.
COPY crates/ ./crates/
RUN find crates -type f -name '*.rs' -exec touch {} + \
    && cargo build --release -p xmedia-bot

# ---------- runtime stage ----------
FROM debian:bookworm-slim

# ARG scope is per-stage: re-declare for the label below.
ARG APP_NAME=telegram-twitter-media-bot

LABEL maintainer="admin@yoursfunny.top"
LABEL org.opencontainers.image.title="${APP_NAME}"

# Everything is copied in — no apt in the runtime stage. Privilege dropping is
# done by docker-entrypoint.sh with setpriv (util-linux, already in
# bookworm-slim), so no gosu needed.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /usr/lib/x86_64-linux-gnu/libssl.so.3* /usr/lib/x86_64-linux-gnu/
COPY --from=builder /usr/lib/x86_64-linux-gnu/libcrypto.so.3* /usr/lib/x86_64-linux-gnu/
COPY --from=builder /usr/local/bin/ffmpeg /usr/local/bin/ffmpeg

WORKDIR /app
COPY --from=builder /build/target/release/xmedia-bot /usr/local/bin/xmedia-bot
COPY docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod a+x /app/docker-entrypoint.sh

# State lives in /app/data (SQLite task queue + chat state); mount a volume
# there to keep it across restarts.
ENTRYPOINT ["/app/docker-entrypoint.sh"]
CMD ["xmedia-bot"]
