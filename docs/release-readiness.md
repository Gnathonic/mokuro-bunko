# Release Readiness Checklist

Use this checklist before tagging a release.

## Product and docs

- Version and changelog are updated.
- README feature list matches implemented behavior.
- Deployment docs match current distribution model (source, pip, local Docker build).
- New configuration keys are documented in config example and docs.
- `mokuro-bunko config check` validates SSL files and key timeout relationships.
- `mokuro-bunko config check` validates SSL cert/key integrity and CORS origin pattern syntax.

## Security

- Auth, registration, and request throttling are enabled and documented.
- Admin API mutating endpoints are protected by role checks and same-origin checks.
- Admin write throttling returns actionable retry metadata (body + rate-limit headers).
- Reverse proxy guidance includes host and forwarding header requirements.
- Path traversal protections are enforced in WebDAV path mapping.

## Reliability

- Startup validation checks storage and SSL prerequisites.
- OCR worker startup and shutdown lifecycle is clean.
- Background watchers and cache refreshers are stopped on shutdown.
- OCR API surfaces failure details (`error`) and recent timestamps for active/last jobs.
- OCR history retention is bounded and persisted safely.
- File uploads commit atomically (no partial file replacement on interruption).
- Concurrent conflicting writes to the same WebDAV resource, or overlapping folder/file paths, return lock errors instead of racing.
- Lock-contention events are audit-logged for operational visibility.
- PROPFIND cache is invalidated immediately on write operations and refreshed in background.

## Storage and data

- Database schema changes are reflected in migration/init code.
- Audit log retention behavior is documented.
- Audit pruning is throttled (periodic) instead of running on every audit write.
- Invite expiry cleanup and upload ownership tracking are verified.

## Operations

- Health and stats endpoints return expected fields.
- TLS settings are validated for enabled deployments.
- Reverse proxy and CORS settings are reviewed for production domain(s).

## Final verification (manual, when explicitly requested)

- Run unit, integration, and e2e tests.
- Validate CLI commands used in docs.
- Smoke-test setup, login, upload, OCR processing, and catalog browsing.
