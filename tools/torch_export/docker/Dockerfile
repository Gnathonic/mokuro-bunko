# Build environment for Linux AOTInductor packages: glibc 2.28 (AlmaLinux 8, the
# manylinux_2_28 baseline libtorch 2.13 itself is built for) + gcc-toolset (its newer
# libstdc++ parts are linked statically, so packages need only GCC 8's libstdc++).
# The generated wrapper .so is compiled and linked here, so it loads on any distro
# libtorch loads on. Python venvs live in the mounted work dir, not in the image.
FROM quay.io/pypa/manylinux_2_28_x86_64@sha256:1e102f2e9ed9b7fd8d64b29f798c76078c3fa8f14d34914d7a4045e39419d8e0
COPY uv /usr/local/bin/uv
ENV PATH=/opt/python/cp312-cp312/bin:$PATH \
    UV_PYTHON=/opt/python/cp312-cp312/bin/python3.12 \
    UV_LINK_MODE=copy \
    HF_HUB_OFFLINE=1
