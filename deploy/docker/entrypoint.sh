#!/bin/sh
# Entrypoint of the Debian/Ubuntu-based images (full, cuda).
#
# 1. Optional nginx X-Accel-Redirect download offload (MOKURO_NGINX_ACCEL=1|true):
#    nginx takes the public ${MOKURO_PORT} and serves library files with sendfile();
#    mokuro-bunko moves to 127.0.0.1:${MOKURO_BACKEND_PORT} and answers library GETs
#    with "X-Accel-Redirect: /internal-library/<path>". Same topology as 0.5.
# 2. OCR backend pack: MOKURO_TORCH_PACK names the pack baked into the image; it is
#    dropped when that directory has no pack (a CUDA image built with BAKE_PACK=0), so
#    the server finds packs under ${MOKURO_STORAGE}/backends. OCR_AUTO_INSTALL=true
#    (0.5.2's variable) runs `mokuro-bunko install-ocr --no-models` as PUID:PGID before
#    the server starts: a no-op when a pack is there, otherwise it downloads the one
#    for this machine into ${MOKURO_STORAGE}/backends (persisted).
# 3. exec bunko-init, which applies PUID/PGID/UMASK/TAKE_OWNERSHIP and execs the
#    server (see packaging/docker-init).
set -eu

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
export MOKURO_HOST="${MOKURO_HOST:-0.0.0.0}"
export MOKURO_PORT="${MOKURO_PORT:-8080}"
export MOKURO_STORAGE="${MOKURO_STORAGE:-/data}"

case "${MOKURO_NGINX_ACCEL:-}" in
1 | true | TRUE | True | yes)
	if [ "$(id -u)" != "0" ]; then
		echo "[entrypoint] MOKURO_NGINX_ACCEL needs the container to start as root; nginx not started" >&2
	else
		export MOKURO_BACKEND_PORT="${MOKURO_BACKEND_PORT:-8081}"
		export MOKURO_LIBRARY="${MOKURO_STORAGE}/library"

		# nginx's `user` directive takes names: make sure PUID/PGID have some.
		if ! getent group "${PGID}" >/dev/null 2>&1; then
			groupadd -o -g "${PGID}" appgroup
		fi
		if ! getent passwd "${PUID}" >/dev/null 2>&1; then
			useradd -o -u "${PUID}" -g "${PGID}" -M -d /tmp -s /usr/sbin/nologin appuser 2>/dev/null ||
				useradd -o -u "${PUID}" -g "${PGID}" -M -d /tmp -s /usr/sbin/nologin appuser
		fi
		nginx_user="$(getent passwd "${PUID}" | cut -d: -f1)"
		nginx_group="$(getent group "${PGID}" | cut -d: -f1)"

		mkdir -p "${MOKURO_LIBRARY}" /tmp/nginx-client-body /tmp/nginx-proxy \
			/tmp/nginx-fastcgi /tmp/nginx-uwsgi /tmp/nginx-scgi
		chown "${PUID}:${PGID}" "${MOKURO_STORAGE}" "${MOKURO_LIBRARY}" /tmp/nginx-client-body \
			/tmp/nginx-proxy /tmp/nginx-fastcgi /tmp/nginx-uwsgi /tmp/nginx-scgi 2>/dev/null || true

		# shellcheck disable=SC2016 # the variable names are for envsubst, not the shell
		envsubst '${MOKURO_PORT} ${MOKURO_BACKEND_PORT} ${MOKURO_LIBRARY}' \
			</etc/nginx/nginx-internal.conf.template >/tmp/nginx.conf

		echo "[entrypoint] nginx X-Accel offload: public :${MOKURO_PORT} -> backend 127.0.0.1:${MOKURO_BACKEND_PORT}"
		# The master stays root (it opens /dev/stdout and /dev/stderr); the workers
		# run as PUID:PGID, which owns the library files.
		nginx -c /tmp/nginx.conf -g "user ${nginx_user} ${nginx_group};"

		export MOKURO_HOST="127.0.0.1"
		export MOKURO_PORT="${MOKURO_BACKEND_PORT}"
		export MOKURO_NGINX_ACCEL="1"
		export BUNKO_NGINX_RUNNING="1"
	fi
	;;
*)
	unset MOKURO_NGINX_ACCEL
	;;
esac

if [ -n "${MOKURO_TORCH_PACK:-}" ] && [ ! -f "${MOKURO_TORCH_PACK}/pack.json" ]; then
	unset MOKURO_TORCH_PACK
fi

case "${OCR_AUTO_INSTALL:-}" in
1 | true | TRUE | True | yes)
	if [ "${1:-serve}" = "serve" ]; then
		echo "[entrypoint] OCR_AUTO_INSTALL: mokuro-bunko install-ocr --no-models"
		/opt/mokuro-bunko/bunko-init install-ocr --no-models ||
			echo "[entrypoint] install-ocr failed; the server starts without a GPU backend (see the log above)" >&2
	fi
	;;
esac

exec /opt/mokuro-bunko/bunko-init "$@"
