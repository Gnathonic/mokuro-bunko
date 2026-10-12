# bunko-server module contract

The server crate is built by several agents in parallel. To avoid edit conflicts:

- **Owned by the orchestrator:** `lib.rs`, `core.rs`, `auth/`, `http/`, `serve.rs`,
  `tls.rs`, `app.rs` (router assembly and service construction) and `Cargo.toml`.
  If you need a dependency, list it in your report. If you cannot build without it, add
  it to `crates/bunko-server/Cargo.toml` under a comment naming your module; that is the
  only shared file you may touch.
- **Each module owns one directory** under `src/` (e.g. `src/accounts/`, `src/admin/`)
  and exposes:
  - `pub struct <Name>Deps { … }`: the handles it needs (a `Core` plus service handles);
  - a router with its state already applied (`.with_state(...)`), which the orchestrator
    merges in front of the WebDAV fallback in 0.5.2's precedence order (spec http-webdav
    §2.1 / §2.4). `accounts`, `admin` and `library` expose
    `pub fn router(deps: <Name>Deps) -> axum::Router`; OCR builds an `OcrControl` with
    `OcrControl::new(OcrDeps)` and exposes `ocr::processor_router` and
    `ocr::queue_router`. `ops/` and `thumbs/` have no router;
  - background services: `UpdateService` (`admin/`), `TunnelService` and
    `DynDnsService` (`ops/`), `OcrControl`, `LibraryRuntime` and `Thumbnails::spawn`.
    `UpdateService::start`, `OcrControl::start` and `Thumbnails::spawn` take a
    `CancellationToken`; the other `start` methods manage their own cancellation.
- **Identity:** use the `RequestCtx` extractor (`crate::core::RequestCtx`). It resolves
  the client IP behind trusted proxies and authenticates Bearer/Basic with the WebDAV
  limiter. Endpoints that 0.5.2 rate-limits with the *login* limiter check the password
  themselves: key `ip:username` (`client.limiter_key`), `core.login_limiter.allow`
  (429 if refused), `db.authenticate_user` in `spawn_blocking`, then `record_success` or
  `record_failure`. `accounts/account.rs` wraps this, plus Bearer tokens, in its own
  `authenticate()`.
- **Database:** `bunko_db::Database` (sync). Call it inside `tokio::task::spawn_blocking`
  (cheap queries are fine inline only if the spec shows they are on a hot path and take
  under a millisecond; prefer spawn_blocking).
- **JSON:** exact field names, status codes and error strings from the specs and
  `spec/web-frontend-contract.md`. Errors from the admin module (CSRF, validation,
  internal) are JSON `{"error": ...}`. Auth refusals on `/_admin/api` (401, 403, 429)
  come earlier, from the auth gate in `app.rs`, as `text/plain` like 0.5.2's
  AuthMiddleware. Python's `json.dumps` default spacing (`", "` / `": "`) and ASCII
  escaping apply where the spec says a test or a client compares bytes; otherwise
  serde_json compact output is fine.
- **Static UI files:** `crate::http::static_files::serve(module, file, cache_control)`
  serves the embedded `web/<module>/` tree.
- **Tests:** `crates/bunko-server/tests/<module>_*.rs`, driving the router with
  `tower::ServiceExt::oneshot` against a temp storage directory and a real
  `bunko_db::Database`.
