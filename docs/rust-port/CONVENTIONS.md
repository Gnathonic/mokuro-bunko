# Rust port — conventions for implementers

Read `ARCHITECTURE.md` first. The behaviour to reproduce is in `spec/*.md`. The Python
0.5.2 source is not in this branch; read it in a detached worktree of `199cff5` at
`../ref-0.5.2` (`git worktree add --detach ../ref-0.5.2 199cff5`) whenever the spec is
unclear — the source wins over the spec. For library paths and the folder-keyed catalog
the reference is 0.5.3 (`4016476`, at `../ref-0.5.3`).

## Ground rules

- **Upgrades from 0.5.x** keep working: the Rust server opens a 0.5.x SQLite file,
  `config.yaml`, library tree and sidecars and upgrades them in place, and keeps the
  HTTP paths, JSON field names, status codes and error strings the web UIs and Mokuro
  Reader depend on. Only that direction: going back to 0.5.x is not supported, so what
  the Rust server writes (the database schema included) may change with a migration and
  need not stay readable by 0.5.x.
- **Removed:** mokuro/manga-ocr, ctd, animetext, rtdetr, Python venvs, Python PyTorch
  and the Python OCR installer, `EngineProcess`/multiprocessing. Never reintroduce GPL
  code or models. libtorch remains, but only in the `bunko-torch` backend pack
  (cpu/cu130/rocm7.1) that `install-ocr` downloads and the binary loads at run time for
  hayai-nova and paddle-manga; the main binary never links it.
- **Memory matters** (lite build must run in 1 GB): stream files, never read a whole
  archive or upload into memory, bound every cache, avoid per-request big allocations.
- Edition 2024, MSRV 1.88. `cargo clippy` clean (`-D warnings` for your crate).
- Errors: `thiserror` enums per library crate. `anyhow` only in the `mokuro-bunko`
  binary, `xtask`, tests, and the top-level crates `bunko-server`, `bunko-tray` and
  `bunko-android`.
  No `unwrap()`/`expect()` outside tests except on provably-infallible invariants (with
  a comment).
- Logging via `tracing` (`info!`, `warn!`...). Never `println!` outside the CLI.
- Blocking work (SQLite, fs walks, zip, image decode) is sync code; async callers wrap it
  in `tokio::task::spawn_blocking`. Library crates other than `bunko-server`,
  `bunko-processor` and `bunko-update` should be sync (no tokio dependency) unless the
  crate doc says otherwise.
- Time: store and compare the formats 0.5.2 writes, and check each column. Most columns
  are SQLite `datetime('now')` text (`YYYY-MM-DD HH:MM:SS`, UTC); `invites.expires_at`
  is Python `isoformat()` of naive local time (`T` separator, `.ffffff` unless zero);
  `auth_tokens` timestamps and the catalog mtimes are REAL epoch seconds (see
  `bunko-db` `pytime.rs`).
- Keep modules small and named after what they do; doc comments say *why*.

## Dependencies

Prefer the versions pinned in the root `Cargo.toml` `[workspace.dependencies]`
(`dep.workspace = true`). If you need a crate that is not there, add it to **your
crate's** `Cargo.toml` with an explicit version; do not edit the root manifest (several
agents work in parallel — tell the orchestrator in your report instead). Licences must
be MIT/Apache-2.0/BSD/ISC/Zlib/MPL-2.0/Unicode/IJG — no GPL/LGPL/AGPL.

## Building in parallel

Several agents build at once. Always use your own target dir to avoid lock contention:

```
CARGO_TARGET_DIR=target/agent-<crate> cargo test -p <crate>
```

Never touch another crate's files except to read them. Do not commit; the orchestrator
reviews and commits.

## Tests and golden data

- Unit tests next to the code; integration tests in `crates/<crate>/tests/`.
- A Python 0.5.2 reference environment exists at `~/.cache/mokuro-bunko-demo/ref052`
  (`~/.cache/mokuro-bunko-demo/ref052/bin/python` imports `mokuro_bunko` as an editable
  install of `../ref-0.5.2/src`; recreate it the way `ref053` is recreated below). Use it
  to generate golden fixtures (e.g. create a DB with the Python code and open it from
  Rust; compile series.json with Python and compare bytes). Put generator scripts in
  `crates/<crate>/tests/golden/` with the fixtures they produce, so they can be
  regenerated.
- For library paths and the folder-keyed catalog the reference is 0.5.3: a Python 0.5.3
  environment at `~/.cache/mokuro-bunko-demo/ref053` (a uv venv with `mokuro_bunko`
  installed editable from the scratch worktree `../ref-0.5.3`, a detached checkout of
  commit `4016476`; never check it out in a main worktree) regenerates the 0.5.3 golden
  DB (`bunko-db/tests/golden/py053.*`). Recreate it with `git worktree add --detach
  ../ref-0.5.3 4016476` and `uv venv --python 3.12 ~/.cache/mokuro-bunko-demo/ref053`
  then `VIRTUAL_ENV=~/.cache/mokuro-bunko-demo/ref053 uv pip install -e ../ref-0.5.3
  pytest httpx`. No test runs the Python code; the fixtures are checked in.
- Keep `/tmp` usage small (it is a RAM disk); put large scratch data under
  `~/.cache/mokuro-bunko-demo/tmp`.
- Real manga samples with shipped `.mokuro` files are in `~/Downloads` (read-only; never
  modify them).

## Report

End with: what you built (public API summary), what is tested and how, what you could
not do or deliberately deviated from (with reasons), and dependencies you added.
