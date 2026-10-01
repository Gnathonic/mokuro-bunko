# syntax=docker/dockerfile:1.7
#
# mokuro-bunko FULL image (CPU): the server plus local OCR on ONNX Runtime, the
# `processor` subcommand, and an optional bundled nginx for the X-Accel download
# offload (MOKURO_NGINX_ACCEL=1, as in the 0.5 images).
#
#   docker build -f deploy/docker/Dockerfile -t mokuro-bunko .
#   docker run -p 8080:8080 -v ./data:/data mokuro-bunko
#
# BIN_FROM=source (default) compiles in the build stage; the release workflow uses
# BIN_FROM=prebuilt (dist/docker/<arch>/full/ from `xtask docker-context`).
# FLAVOR=lite builds this Debian+nginx image around the lite binary instead.

ARG BIN_FROM=source
ARG FLAVOR=full

# bookworm's glibc (2.36) keeps the binary runnable on the newer runtime bases.
FROM rust:1-slim-bookworm AS source
ARG FLAVOR
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates gcc g++ make perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/root/.cache/ort.pyke.io \
    --mount=type=cache,target=/src/target \
    set -eux; \
    host="$(rustc -vV | sed -n 's/^host: //p')"; \
    cargo run --release --locked -p xtask -- dist --locked --target "$host" --flavor "$FLAVOR" --out /tmp/dist; \
    mkdir -p /out; \
    tar -xzf /tmp/dist/*.tar.gz -C /out --strip-components=1; \
    cargo build --release --locked --manifest-path packaging/docker-init/Cargo.toml --target-dir target/docker-init; \
    cp target/docker-init/release/bunko-init /out/

FROM scratch AS prebuilt
ARG TARGETARCH
ARG FLAVOR
COPY dist/docker/${TARGETARCH}/${FLAVOR}/ /out/

FROM ${BIN_FROM} AS bin

FROM debian:trixie-slim
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/Gnathonic/mokuro-bunko" \
      org.opencontainers.image.title="mokuro-bunko" \
      org.opencontainers.image.description="Self-hosted manga library server for Mokuro Reader with local OCR (ONNX Runtime, CPU)" \
      org.opencontainers.image.licenses="MPL-2.0" \
      org.opencontainers.image.version="${VERSION}"

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini nginx gettext-base \
    && rm -rf /var/lib/apt/lists/* /etc/nginx/sites-enabled/default

COPY --from=bin /out/ /opt/mokuro-bunko/
COPY deploy/nginx-internal.conf.template /etc/nginx/nginx-internal.conf.template
COPY deploy/docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV PATH="/opt/mokuro-bunko:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    MOKURO_INSTALL_KIND="docker" \
    MOKURO_HOST="0.0.0.0" \
    MOKURO_PORT="8080" \
    MOKURO_BACKEND_PORT="8081" \
    MOKURO_STORAGE="/data" \
    MOKURO_CONFIG="/data/config.yaml" \
    MOKURO_REGISTRATION_MODE="disabled" \
    MOKURO_SSL_ENABLED="false" \
    MOKURO_NGINX_ACCEL="0" \
    PUID="1000" \
    PGID="1000" \
    UMASK="002"

VOLUME ["/data"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD ["mokuro-bunko", "healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
