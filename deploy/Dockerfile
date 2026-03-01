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

# Copy project files
COPY pyproject.toml .
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

# Create non-root user
RUN useradd --create-home --shell /bin/bash mokuro

WORKDIR /app

# Copy virtual environment from builder
COPY --from=builder /opt/venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"

# Create storage directory
RUN mkdir -p /data && chown -R mokuro:mokuro /data

# Environment variables for configuration
ENV MOKURO_HOST="0.0.0.0"
ENV MOKURO_PORT="8080"
ENV MOKURO_STORAGE="/data"
ENV MOKURO_REGISTRATION_MODE="disabled"
ENV MOKURO_SSL_ENABLED="false"

# Switch to non-root user
USER mokuro

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD python -c "import httpx; httpx.get('http://localhost:${MOKURO_PORT}/', timeout=5)" || exit 1

# Entry point
ENTRYPOINT ["mokuro-bunko"]
CMD ["serve"]
