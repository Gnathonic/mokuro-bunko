# syntax=docker/dockerfile:1
#
# Mokuro Bunko Server Docker Image
#
# Usage:
#   docker build -t mokuro-bunko .
#   docker run -p 8080:8080 -v ./storage:/data mokuro-bunko
#

FROM python:3.12-slim as builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc \
    libffi-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy project files (README.md is referenced by pyproject's readme field)
COPY pyproject.toml .
COPY README.md .
COPY src/ src/

# Build wheel
RUN pip install --no-cache-dir build && \
    python -m build --wheel

# Install dependencies into a virtual environment
RUN python -m venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"
RUN pip install --no-cache-dir dist/*.whl


FROM python:3.12-slim as runtime

LABEL org.opencontainers.image.source="https://github.com/nathan/mokuro-bunko"
LABEL org.opencontainers.image.title="Mokuro Bunko Server"
LABEL org.opencontainers.image.description="WebDAV server for Mokuro reader with shared manga library"
LABEL org.opencontainers.image.licenses="MIT"

# Install nginx (download offload via X-Accel-Redirect) and envsubst.
RUN apt-get update && apt-get install -y --no-install-recommends \
    nginx gettext-base \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd --create-home --shell /bin/bash mokuro

WORKDIR /app

# Copy virtual environment from builder
COPY --from=builder /opt/venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"

# nginx config template + entrypoint that fronts Python with nginx
COPY deploy/nginx-internal.conf.template /etc/nginx/nginx-internal.conf.template
COPY deploy/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Storage + nginx runtime dirs writable by the non-root user
RUN mkdir -p /data /tmp/nginx-client-body /tmp/nginx-proxy \
    && chown -R mokuro:mokuro /data /tmp/nginx-client-body /tmp/nginx-proxy \
    && chown -R mokuro:mokuro /var/log/nginx /var/lib/nginx

# Environment variables for configuration
ENV MOKURO_HOST="0.0.0.0"
ENV MOKURO_PORT="8080"
ENV MOKURO_BACKEND_PORT="8081"
ENV MOKURO_STORAGE="/data"
ENV MOKURO_REGISTRATION_MODE="disabled"
ENV MOKURO_SSL_ENABLED="false"
# nginx fronts Python and serves downloads via X-Accel-Redirect
ENV MOKURO_NGINX_ACCEL="1"

# Switch to non-root user
USER mokuro

# Expose the public nginx port
EXPOSE 8080

# Health check (hits nginx, which proxies to Python)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD python -c "import httpx; httpx.get('http://localhost:${MOKURO_PORT}/', timeout=5)" || exit 1

# Entry point: render nginx config, start nginx, then exec the server
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
