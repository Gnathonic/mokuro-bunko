#!/bin/sh
# Entrypoint of the full image (Debian-based; also published as the `-cuda` alias).
#
# 1. Optional nginx X-Accel-Redirect download offload (MOKURO_NGINX_ACCEL=1|true):
#    nginx takes the public ${MOKURO_PORT} and serves library files with sendfile();
#    mokuro-bunko moves to 127.0.0.1:${MOKURO_BACKEND_PORT} and answers library GETs
#    with "X-Accel-Redirect: /internal-library/<path>". Same topology as 0.5.
# 2. OCR backend, on demand: before `serve` (and `processor serve`) it runs
#    `mokuro-bunko install-ocr --if-needed` as PUID:PGID. The image carries no pack and
#    no model; that command detects the GPUs this container was given, applies
#    ocr.backend (MOKURO_OCR_BACKEND) and installs the matching pack into
#    ${MOKURO_STORAGE}/backends and the enabled engines' models into
#    ${MOKURO_STORAGE}/models (persisted). It does nothing when local OCR is off
#    (ocr.backend: skip, ocr.local_processing: false), when MOKURO_OCR_AUTO_INSTALL is
#    false, or when a fitting pack of this release is already there. A failure is
#    logged and the server starts anyway (without the recognizer engines).
# 3. exec bunko-init, which applies PUID/PGID/UMASK/TAKE_OWNERSHIP (keeping the GPU
#    device groups) and execs the server (see packaging/docker-init).
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

# A pack pinned by hand that is not there would leave OCR without a backend.
if [ -n "${MOKURO_TORCH_PACK:-}" ] && [ ! -f "${MOKURO_TORCH_PACK}/pack.json" ]; then
	echo "[entrypoint] MOKURO_TORCH_PACK=${MOKURO_TORCH_PACK} holds no pack.json; ignored" >&2
	unset MOKURO_TORCH_PACK
fi

ocr_install() {
	echo "[entrypoint] OCR backend: mokuro-bunko install-ocr --if-needed $*"
	/opt/mokuro-bunko/bunko-init install-ocr --if-needed "$@" ||
		echo "[entrypoint] install-ocr failed; the server starts without the recognizer engines (see above; retried on the next start)" >&2
}

case "${1:-serve}" in
serve)
	ocr_install
	;;
processor)
	if [ "${2:-}" = "serve" ]; then
		# The processor's config: --config PATH / --config=PATH, else MOKURO_PROCESSOR_CONFIG.
		pconf="${MOKURO_PROCESSOR_CONFIG:-}"
		prev=""
		for a in "$@"; do
			case "$a" in
			--config=*) pconf="${a#--config=}" ;;
			esac
			if [ "$prev" = "--config" ]; then
				pconf="$a"
			fi
			prev="$a"
		done
		(
			MOKURO_PROCESSOR_CONFIG="$pconf"
			export MOKURO_PROCESSOR_CONFIG
			ocr_install --processor
		)
	fi
	;;
esac

exec /opt/mokuro-bunko/bunko-init "$@"
