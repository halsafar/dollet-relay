# syntax=docker/dockerfile:1

# The SPA is embedded into the binary at compile time, so it must be built
# first and copied into the Rust stage.
FROM docker.io/library/node:22-alpine AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build


# cargo-chef caches the dependency build in its own layer, so touching source
# does not rebuild ~400 crates.
FROM docker.io/library/rust:1.98-trixie AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY migrations/ migrations/
COPY --from=web /web/dist/ web/dist/
RUN cargo build --release --bin dollet


FROM docker.io/library/debian:trixie-slim AS runtime
# VA-API drivers cover AMD/Intel hardware transcoding for output profiles.
# intel-media-va-driver is the DFSG build from main; the -non-free variant
# needs an extra apt component and buys nothing used here.
#
# tini because `dollet` is PID 1 here. When a stream profile shaped
# `sh -c "streamlink | ffmpeg"` is terminated, the pipeline's grandchildren
# reparent to PID 1 — and tokio only reaps children it spawned itself, while
# `dollet` must not `waitpid(-1)` or it steals those results from tokio.
# Without an init that reaps orphans, every failover leaks a zombie.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ffmpeg \
      ca-certificates \
      tini \
      va-driver-all \
      intel-media-va-driver \
      mesa-va-drivers \
    && rm -rf /var/lib/apt/lists/*

# `--device /dev/dri` mounts nodes owned by `root:video` / `root:render` at
# 0660, so a uid in neither group cannot open them and every hardware transcode
# fails with EACCES. This covers the case where the host's gids match the
# image's; where they do not, compose's `group_add:` is the fix and the server
# says so at startup.
RUN (getent group render >/dev/null || groupadd --system render) \
    && useradd --system --uid 1000 --create-home --shell /usr/sbin/nologin dollet \
    && usermod --append --groups video,render dollet \
    && mkdir -p /data && chown dollet:dollet /data

COPY --from=builder /build/target/release/dollet /usr/local/bin/dollet

USER dollet
WORKDIR /data
VOLUME ["/data"]
EXPOSE 9191

ENV DOLLET_DATA_DIR=/data \
    DOLLET_LISTEN=0.0.0.0:9191 \
    DOLLET_LOG=info

# `dollet health` asks the running server; `--version` succeeds even when the
# server has stopped serving.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD ["/usr/local/bin/dollet", "health"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/dollet"]
CMD ["serve"]
