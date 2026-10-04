# syntax=docker/dockerfile:1.7
#
# mokuro-bunko FULL image (CPU): the server, local OCR and the `processor` command,
# with the CPU OCR backend pack (libtorch 2.13.0 + libbunko_torch) built in, plus an
# optional nginx for the X-Accel download offload (MOKURO_NGINX_ACCEL=1, as in 0.5).
# Successor of the 0.5.2 `deploy/Dockerfile` image.
#
#   docker build -f deploy/docker/Dockerfile -t mokuro-bunko .
#   docker run -p 8080:8080 -v ./data:/data mokuro-bunko
#
# OCR works out of the box: the recognizers run on the baked-in pack
# (MOKURO_TORCH_PACK); the compiled model packages and ONNX models are downloaded on
# first use into /data/models (persisted with the library).
#
# BIN_FROM=source (default) compiles in the build stage (and downloads libtorch from
# download.pytorch.org); the release workflow uses BIN_FROM=prebuilt
# (dist/docker/<arch>/full/ from `xtask docker-context`: the signed release binary and
# the released CPU pack). linux/amd64 (the packs are x86_64; see PACKAGING.md §2).

ARG BIN_FROM=source

# trixie = the runtime base's glibc (2.41); ONNX Runtime 1.28's static libraries need
# libstdc++ from GCC >= 13 (bookworm has 12).
FROM rust:1-slim-trixie AS source
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl gcc g++ make perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/root/.cache/ort.pyke.io \
    --mount=type=cache,target=/root/.cache/torch-pack \
    --mount=type=cache,target=/src/target \
    set -eux; \
    host="$(rustc -vV | sed -n 's/^host: //p')"; \
    cargo run --release --locked -p xtask -- dist --locked --target "$host" --flavor full --out /tmp/dist; \
    mkdir -p /out/backends; \
    tar -xzf /tmp/dist/*.tar.gz -C /out --strip-components=1; \
    BUNKO_PACK_CACHE=/root/.cache/torch-pack cargo run --release --locked -p xtask -- \
        torch-pack --locked --variant cpu --no-archive --out /out/backends; \
    cargo build --release --locked --manifest-path packaging/docker-init/Cargo.toml --target-dir target/docker-init; \
    cp target/docker-init/release/bunko-init /out/

FROM scratch AS prebuilt
ARG TARGETARCH
COPY dist/docker/${TARGETARCH}/full/ /out/

FROM ${BIN_FROM} AS bin

FROM debian:trixie-slim
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/Gnathonic/mokuro-bunko" \
      org.opencontainers.image.title="mokuro-bunko" \
      org.opencontainers.image.description="Self-hosted manga library server for Mokuro Reader with local OCR (libtorch, CPU)" \
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
    MOKURO_TORCH_PACK="/opt/mokuro-bunko/backends/torch-cpu-2.13.0" \
    PUID="1000" \
    PGID="1000" \
    UMASK="002"

VOLUME ["/data"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD ["mokuro-bunko", "healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
