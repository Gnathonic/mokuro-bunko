#!/usr/bin/env bash
set -euo pipefail

# Unraid-friendly defaults
PUID="${PUID:-99}"
PGID="${PGID:-100}"
UMASK="${UMASK:-002}"
TAKE_OWNERSHIP="${TAKE_OWNERSHIP:-false}"
OCR_AUTO_INSTALL="${OCR_AUTO_INSTALL:-false}"

export MOKURO_HOST="${MOKURO_HOST:-0.0.0.0}"
export MOKURO_PORT="${MOKURO_PORT:-8080}"
export MOKURO_STORAGE="${MOKURO_STORAGE:-/data}"
export MOKURO_OCR_BACKEND="${MOKURO_OCR_BACKEND:-auto}"
export MOKURO_CONFIG="${MOKURO_CONFIG:-/config/config.yaml}"
export MOKURO_BUNKO_OCR_ENV="${MOKURO_BUNKO_OCR_ENV:-/opt/ocr-env}"

# Redirect pip temp/cache to the data volume so large downloads (e.g. CUDA
# PyTorch ~4 GB) don't fill the container's root filesystem.
export TMPDIR="${TMPDIR:-${MOKURO_STORAGE}/.tmp}"
export PIP_CACHE_DIR="${PIP_CACHE_DIR:-${MOKURO_STORAGE}/.pip-cache}"

mkdir -p "${MOKURO_STORAGE}" /config "${TMPDIR}" "${PIP_CACHE_DIR}"

if ! getent group "${PGID}" >/dev/null 2>&1; then
  groupadd -o -g "${PGID}" appgroup
fi

if ! getent passwd "${PUID}" >/dev/null 2>&1; then
  useradd -o -u "${PUID}" -g "${PGID}" -M -d /tmp -s /usr/sbin/nologin appuser
fi

if [ "${TAKE_OWNERSHIP}" = "true" ]; then
  echo "[entrypoint] Fixing ownership to ${PUID}:${PGID} on ${MOKURO_STORAGE} and /config ..."
  chown -R "${PUID}:${PGID}" "${MOKURO_STORAGE}" /config
  echo "[entrypoint] Ownership fix complete."
fi

# Ensure the container user can write to directories it manages, even when
# TAKE_OWNERSHIP is false.  This covers the common Unraid case where the
# share already has the right GID but individual files may not be
# group-writable.
# Ensure writable directories for the app user.  For the OCR env parent we
# just need the user to be able to create the directory (venv.create handles
# the rest).
chown "${PUID}:${PGID}" "${MOKURO_STORAGE}" /config "${TMPDIR}" "${PIP_CACHE_DIR}" 2>/dev/null || true
ocr_env_parent="$(dirname "${MOKURO_BUNKO_OCR_ENV}")"
chown "${PUID}:${PGID}" "${ocr_env_parent}" 2>/dev/null || true
# Also chown the OCR env directory itself (may be a Docker volume mount owned by root)
if [ -d "${MOKURO_BUNKO_OCR_ENV}" ]; then
  chown "${PUID}:${PGID}" "${MOKURO_BUNKO_OCR_ENV}" 2>/dev/null || true
fi

umask "${UMASK}"

# Optional one-shot OCR env setup at container start.
if [ "${OCR_AUTO_INSTALL}" = "true" ] && [ "${MOKURO_OCR_BACKEND}" != "skip" ]; then
  gosu "${PUID}:${PGID}" mokuro-bunko install-ocr --backend "${MOKURO_OCR_BACKEND}" || true
fi

# Optional nginx X-Accel-Redirect download offload.
#
# When MOKURO_NGINX_ACCEL=1, run nginx in front of the Python/cheroot backend
# so large library downloads are served by nginx via sendfile() instead of
# holding a cheroot worker thread for the whole transfer (the main driver of
# the 503 / thread-pool-exhaustion issue under download load, cf. commit
# 8d98297). Default is OFF, which preserves the original single-process
# topology; enable it from the Unraid template once validated.
# See deploy/nginx-internal.conf.template.
if [ "${MOKURO_NGINX_ACCEL:-}" = "1" ] || [ "${MOKURO_NGINX_ACCEL:-}" = "true" ]; then
  export MOKURO_NGINX_ACCEL="1"
  export MOKURO_BACKEND_PORT="${MOKURO_BACKEND_PORT:-8081}"
  export MOKURO_LIBRARY="${MOKURO_STORAGE}/library"

  # nginx pid + temp paths live under /tmp (see the config template); the
  # unprivileged workers must be able to write the temp dirs.
  mkdir -p "${MOKURO_LIBRARY}" \
    /tmp/nginx-client-body /tmp/nginx-proxy /tmp/nginx-fastcgi \
    /tmp/nginx-uwsgi /tmp/nginx-scgi
  chown "${PUID}:${PGID}" "${MOKURO_LIBRARY}" \
    /tmp/nginx-client-body /tmp/nginx-proxy /tmp/nginx-fastcgi \
    /tmp/nginx-uwsgi /tmp/nginx-scgi 2>/dev/null || true

  # Render the nginx config (public ${MOKURO_PORT} -> backend ${MOKURO_BACKEND_PORT}).
  envsubst '${MOKURO_PORT} ${MOKURO_BACKEND_PORT} ${MOKURO_LIBRARY}' \
    < /etc/nginx/nginx-internal.conf.template \
    > /tmp/nginx.conf

  echo "[entrypoint] nginx X-Accel offload enabled: public :${MOKURO_PORT} -> backend 127.0.0.1:${MOKURO_BACKEND_PORT}"
  # Standard nginx privilege model: the master starts as root so it can open
  # /dev/stdout|stderr for logging (fd 1/2 are owned by root in this
  # root-launched container) and supervise, then drops the request-handling
  # WORKERS to the unprivileged app user via the `user` directive. Launching the
  # master itself via gosu fails with "open() /dev/stderr (13: Permission
  # denied)". Workers run as ${PUID}:${PGID} (which owns the library files);
  # access is further confined by the internal library-root-only alias and
  # Python's path confinement + URL-encoding of the X-Accel-Redirect.
  nginx_user="$(getent passwd "${PUID}" | cut -d: -f1)"
  nginx_group="$(getent group "${PGID}" | cut -d: -f1)"
  nginx -c /tmp/nginx.conf -g "user ${nginx_user} ${nginx_group};"

  # Python now listens only on the internal backend port and emits
  # X-Accel-Redirect for library downloads.
  export MOKURO_HOST="127.0.0.1"
  export MOKURO_PORT="${MOKURO_BACKEND_PORT}"
fi

if [ "$#" -eq 0 ]; then
  set -- serve
fi

exec gosu "${PUID}:${PGID}" mokuro-bunko "$@"
