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

if [ "$#" -eq 0 ]; then
  set -- serve
fi

exec gosu "${PUID}:${PGID}" mokuro-bunko "$@"
