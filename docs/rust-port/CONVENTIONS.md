# Rust port — conventions for implementers

Read `ARCHITECTURE.md` first. The behaviour to reproduce is in `spec/*.md`; the Python
0.5.2 source is in `src/mokuro_bunko/` (read it whenever the spec is unclear — the
source wins over the spec).

## Ground rules

- **Drop-in compatibility** with 0.5.2 storage: the same SQLite file, `config.yaml`,
  library tree, sidecars, and the same HTTP paths, JSON field names, status codes and
  error strings the web UIs and Mokuro Reader depend on. When 0.5.2 has a quirk the spec
  marks "fix", fix it; otherwise reproduce it.
- **Removed:** mokuro/manga-ocr, ctd, animetext, rtdetr, Python venvs/torch/installer,
  `EngineProcess`/multiprocessing. Never reintroduce GPL code or models.
- **Memory matters** (lite build must run in 1 GB): stream files, never read a whole
  archive or upload into memory, bound every cache, avoid per-request big allocations.
- Edition 2024, MSRV 1.88. `cargo clippy` clean (`-D warnings` for your crate).
- Errors: `thiserror` enums per library crate; `anyhow` only in the binary and tests.
  No `unwrap()`/`expect()` outside tests except on provably-infallible invariants (with
  a comment).
- Logging via `tracing` (`info!`, `warn!`...). Never `println!` outside the CLI.
- Blocking work (SQLite, fs walks, zip, image decode) is sync code; async callers wrap it
  in `tokio::task::spawn_blocking`. Library crates other than `bunko-server`,
  `bunko-processor` and `bunko-update` should be sync (no tokio dependency) unless the
  crate doc says otherwise.
- Time: store and compare the same textual formats 0.5.2 writes (the DB holds ISO-8601
  text — check the spec for each column).
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
CARGO_TARGET_DIR=/home/nathan/Projects/mokuro-webdav-library-worktrees/rust-0.7/target/agent-<crate> cargo test -p <crate>
```

Never touch another crate's files except to read them. Do not commit; the orchestrator
reviews and commits.

## Tests and golden data

- Unit tests next to the code; integration tests in `crates/<crate>/tests/`.
- A Python 0.5.2 reference environment exists at `~/.cache/mokuro-bunko-demo/ref052`
  (`~/.cache/mokuro-bunko-demo/ref052/bin/python` imports `mokuro_bunko` from this
  worktree's `src/`). Use it to generate golden fixtures (e.g. create a DB with the Python
  code and open it from Rust; compile series.json with Python and compare bytes). Put
  generator scripts in `crates/<crate>/tests/golden/` with the fixtures they produce, so
  they can be regenerated.
- Production runs 0.5.3 (NTFS-style case-insensitive library paths, the catalog keyed
  by folder), and 0.7 is held to it: a Python 0.5.3 reference environment exists at
  `~/.cache/mokuro-bunko-demo/ref053` (a uv venv with `mokuro_bunko` installed editable
  from the scratch worktree `../ref-0.5.3`, a detached checkout of commit `4016476`;
  never check it out in a main worktree). Recreate it with `git worktree add --detach
  ../ref-0.5.3 4016476` and `uv venv --python 3.12 ~/.cache/mokuro-bunko-demo/ref053`
  then `VIRTUAL_ENV=~/.cache/mokuro-bunko-demo/ref053 uv pip install -e ../ref-0.5.3
  pytest httpx`. It drives `bunko-server/tests/differential_053.rs` and the 0.5.3
  golden DB (`bunko-db/tests/golden/py053.*`).
- Keep `/tmp` usage small (it is a RAM disk); put large scratch data under
  `~/.cache/mokuro-bunko-demo/tmp`.
- Real manga samples with shipped `.mokuro` files are in `~/Downloads` (read-only; never
  modify them).

## Report

End with: what you built (public API summary), what is tested and how, what you could
not do or deliberately deviated from (with reasons), and dependencies you added.
