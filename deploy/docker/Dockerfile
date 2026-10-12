# syntax=docker/dockerfile:1.7
#
# mokuro-bunko FULL image: the server, local OCR and the `processor` command, plus an
# optional nginx for the X-Accel download offload (MOKURO_NGINX_ACCEL=1, as in 0.5).
# Successor of both 0.5.2 images (`deploy/Dockerfile` and the CUDA `Dockerfile.unraid`):
# the release publishes it as `:<ver>` / `:latest` and, for templates written for the
# CUDA image, under the alias `:<ver>-cuda` / `:latest-cuda` (the same image).
#
#   docker build -f deploy/docker/Dockerfile -t mokuro-bunko .
#   docker run -p 8080:8080 -v ./data:/data mokuro-bunko                    # CPU
#   docker run --gpus all -p 8080:8080 -v ./data:/data mokuro-bunko         # NVIDIA
#   docker run --device /dev/kfd --device /dev/dri -p 8080:8080 -v ./data:/data mokuro-bunko  # AMD
#
# No OCR backend pack and no model is inside. On start the entrypoint runs
# `mokuro-bunko install-ocr --if-needed`: unless local OCR is off (ocr.backend: skip,
# ocr.local_processing: false) or MOKURO_OCR_AUTO_INSTALL=false, it looks at the GPUs
# this container was given (NVIDIA through the container toolkit, AMD through
# /dev/kfd + /dev/dri), applies ocr.backend (MOKURO_OCR_BACKEND: auto, cpu, cuda,
# rocm), downloads the matching pack (cpu, cu130 + NVIDIA's libraries from PyPI, or
# rocm7.1) from this release into /data/backends and the enabled engines' models into
# /data/models. Later starts reuse them; a new GPU (or a preference for one this
# container now sees) installs its pack. The packs are self-contained: no CUDA or ROCm
# base image; the NVIDIA driver library comes from the host through the toolkit.
#
# BAKE_PACK=1 (opt-in, for air-gapped self-builds; the release never bakes) builds the
# PACK_VARIANT pack (cpu, cu130 with the NVIDIA libraries, rocm7.1) into
# /opt/mokuro-bunko/backends; it is used when /data/backends has no pack of this
# release, and the automatic install then downloads nothing for that variant.
#
# BIN_FROM=source (default) compiles in the build stage; the release workflow uses
# BIN_FROM=prebuilt (dist/docker/<arch>/full/ from `xtask docker-context`: the signed
# release binary). linux/amd64 (the packs are x86_64; see PACKAGING.md §2).

ARG BIN_FROM=source

# trixie = the runtime base's glibc (2.41); ONNX Runtime 1.28's static libraries need
# libstdc++ from GCC >= 13 (bookworm has 12).
FROM rust:1-slim-trixie AS source
ARG BAKE_PACK=0
ARG PACK_VARIANT=cpu
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
    cargo run --release --locked -p xtask -- dist --locked --no-tray --target "$host" --flavor full --out /tmp/dist; \
    mkdir -p /out; \
    tar -xzf /tmp/dist/*.tar.gz -C /out --strip-components=1; \
    if [ "$BAKE_PACK" = "1" ]; then \
        extra=""; if [ "$PACK_VARIANT" = "cu130" ]; then extra="--bundle-external"; fi; \
        BUNKO_PACK_CACHE=/root/.cache/torch-pack cargo run --release --locked -p xtask -- \
            torch-pack --locked --variant "$PACK_VARIANT" $extra --no-archive --out /out/backends; \
    fi; \
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
      org.opencontainers.image.description="Self-hosted manga library server for Mokuro Reader with local OCR (libtorch: CPU, NVIDIA CUDA or AMD ROCm, downloaded on first start)" \
      org.opencontainers.image.licenses="MPL-2.0" \
      org.opencontainers.image.version="${VERSION}"

# libatomic1, libdw1t64, libelf1t64, libnuma1 (+ the unversioned libnuma.so, which
# libtorch's ROCm build dlopen()s): host libraries the AMD (rocm7.1) pack expects, as
# on a desktop; about 1.5 MB, LGPL/GPL Debian packages like the rest of the base.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini nginx gettext-base \
        libatomic1 libdw1t64 libelf1t64 libnuma1 \
    && ln -s libnuma.so.1 /usr/lib/x86_64-linux-gnu/libnuma.so && ldconfig \
    && rm -rf /var/lib/apt/lists/* /etc/nginx/sites-enabled/default

COPY --from=bin /out/ /opt/mokuro-bunko/
COPY deploy/nginx-internal.conf.template /etc/nginx/nginx-internal.conf.template
COPY deploy/docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# MOKURO_CONFIG: in the data volume (`docker exec ... mokuro-bunko <command>` reads the
# same file). Templates for the 0.5.2 CUDA image set /config/config.yaml themselves.
# NVIDIA_*: what the container toolkit passes in when this image runs with
# --runtime=nvidia (`--gpus all` sets them itself); without the toolkit they do nothing.
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
    MOKURO_OCR_AUTO_INSTALL="true" \
    NVIDIA_VISIBLE_DEVICES="all" \
    NVIDIA_DRIVER_CAPABILITIES="compute,utility" \
    PUID="1000" \
    PGID="1000" \
    UMASK="002"

VOLUME ["/data"]
EXPOSE 8080

# The server answers within seconds of the start; the OCR backend downloads in the
# background meanwhile (it does not hold up the health check).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD ["mokuro-bunko", "healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
