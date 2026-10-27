# The server embeds the committed browser bundle. CI rebuilds it and rejects
# stale assets before qualifying this image. Rust builds need no Node.
FROM rust:1-bookworm AS build
WORKDIR /src
ARG BBS_BUILD_REVISION
ARG BBS_BUILD_DIRTY
ARG CARGO_BUILD_JOBS=4
COPY . .
RUN cargo build --release --workspace --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends sqlite3 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 999 --user-group --create-home --home-dir /var/lib/bbs bbs
COPY --from=build /src/target/release/bbs /usr/local/bin/bbs
COPY --from=build /src/target/release/bbs-server /usr/local/bin/bbs-server
USER 999:999
ENV HOME=/var/lib/bbs BBS_LOG_STDERR=1 SQLITE_TMPDIR=/var/lib/bbs
VOLUME /var/lib/bbs
EXPOSE 7777 7778
# Keep the existing image arguments and CLI overrides for admin and backup.
ENTRYPOINT ["bbs", "serve", "--state-dir", "/var/lib/bbs"]
CMD ["--listen", "0.0.0.0:7777"]
