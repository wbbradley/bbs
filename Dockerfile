# Builds a small image that runs `bbs serve`. The proto compiles with protox,
# so the builder needs no protoc.

# The web UI (`bbs serve --web`), embedded into the binary at build time.
FROM node:22-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
COPY assets/connect/logo.txt /src/assets/connect/logo.txt
RUN npm run build

FROM rust:1-bookworm AS build
WORKDIR /src
# .git is not copied, so the builder passes the source identity (see build.rs).
ARG BBS_BUILD_REVISION
ARG BBS_BUILD_DIRTY
COPY . .
COPY --from=web /src/web/dist /web-dist
# The web stage built the page; build.rs embeds it instead of running npm.
ENV BBS_WEB_DIST=/web-dist
RUN cargo build --release --locked --features web

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends sqlite3 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 999 --user-group --create-home --home-dir /var/lib/bbs bbs
COPY --from=build /src/target/release/bbs /usr/local/bin/bbs
# Numeric, so Kubernetes can verify runAsNonRoot without a runAsUser override.
USER 999:999
# Logs go to stderr for `docker logs`; the state dir is a volume so the database
# survives restarts.
# SQLite may spill inbox sorts to disk even when the root filesystem is read-only.
ENV HOME=/var/lib/bbs BBS_LOG_STDERR=1 SQLITE_TMPDIR=/var/lib/bbs
VOLUME /var/lib/bbs
EXPOSE 7777 7778
ENTRYPOINT ["bbs", "serve", "--state-dir", "/var/lib/bbs"]
CMD ["--listen", "0.0.0.0:7777"]
