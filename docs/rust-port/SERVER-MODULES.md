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
  - `pub fn router(deps: <Name>Deps) -> axum::Router`: a router with its state already
    applied (`.with_state(...)`). The orchestrator merges it in front of the WebDAV
    fallback in 0.5.2's precedence order (spec http-webdav §2.1 / §2.4);
  - for background services: `pub struct <Name>Service` with
    `start(&self, stop: CancellationToken)` and an async `stop()`.
- **Identity:** use the `RequestCtx` extractor (`crate::core::RequestCtx`). It resolves
  the client IP behind trusted proxies and authenticates Bearer/Basic with the WebDAV
  limiter. Endpoints that 0.5.2 rate-limits with the *login* limiter call
  `crate::auth::authenticate(headers, ip, core.backend, &core.login_limiter)` themselves.
- **Database:** `bunko_db::Database` (sync). Call it inside `tokio::task::spawn_blocking`
  (cheap queries are fine inline only if the spec shows they are on a hot path and take
  under a millisecond; prefer spawn_blocking).
- **JSON:** exact field names, status codes and error strings from the specs and
  `spec/web-frontend-contract.md`. Admin mutation responses always carry a JSON body,
  errors included. Python's `json.dumps` default spacing (`", "` / `": "`) and ASCII
  escaping apply where the spec says a test or a client compares bytes; otherwise
  serde_json compact output is fine.
- **Static UI files:** `crate::http::static_files::serve(module, file, cache_control)`
  serves the embedded `web/<module>/` tree.
- **Tests:** `crates/bunko-server/tests/<module>_*.rs`, driving the router with
  `tower::ServiceExt::oneshot` against a temp storage directory and a real
  `bunko_db::Database`.
