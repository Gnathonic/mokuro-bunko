#!/bin/sh
# Generic entrypoint: run nginx in front of the Python (cheroot) backend so
# library downloads are offloaded via X-Accel-Redirect. See
# deploy/nginx-internal.conf.template.
#
# NOTE: this is the reference (generic) image. The Unraid image
# (Dockerfile.unraid / docker-entrypoint.unraid.sh) runs a single gosu'd
# process and would need nginx wired in separately.
set -e

# Library root the X-Accel internal location is aliased to.
export MOKURO_LIBRARY="${MOKURO_STORAGE}/library"
mkdir -p "${MOKURO_LIBRARY}" /tmp/nginx-client-body /tmp/nginx-proxy

# Render the nginx config from the template.
envsubst '${MOKURO_PORT} ${MOKURO_BACKEND_PORT} ${MOKURO_LIBRARY}' \
    < /etc/nginx/nginx-internal.conf.template \
    > /tmp/nginx.conf

# nginx fronts the public port; start it in the background.
nginx -c /tmp/nginx.conf &

# Python now listens only on the internal backend port, and emits
# X-Accel-Redirect for library downloads.
export MOKURO_HOST="127.0.0.1"
export MOKURO_PORT="${MOKURO_BACKEND_PORT}"
export MOKURO_NGINX_ACCEL="1"

exec mokuro-bunko "$@"
