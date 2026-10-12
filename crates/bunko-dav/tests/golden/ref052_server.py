"""Start the Python 0.5.2 (or 0.5.3) reference server over a prepared storage tree.

Serves whichever `mokuro_bunko` the interpreter imports: `tests/differential.rs` runs it
with the 0.5.2 env, `bunko-server/tests/differential_053.rs` with the 0.5.3 one. Creates
the users the request scripts sign in as (password `pass1234` for all) and serves `create_app(config)` -- the full 0.5.2 HTTP
stack -- on cheroot, the way `create_ssl_server` does, but WITHOUT `run_server`'s OCR
worker: its thumbnail loop writes `.nocover`/`.webp` files into the library on its own,
which would make the two trees diverge; the metadata compiler and library watcher are
stopped for the same reason (they write `series.json` / `catalog.json`). With no `OcrControl` the upload middleware sends
no follow-up headers, which is what the Rust side does with `NoHooks`.

    python ref052_server.py <storage base> <port>
"""

from __future__ import annotations

import sys
from pathlib import Path


def main() -> None:
    base = Path(sys.argv[1]).resolve()
    port = int(sys.argv[2])
    from cheroot.wsgi import Server

    from mokuro_bunko.config import Config, ServerConfig, StorageConfig
    from mokuro_bunko.database import Database
    from mokuro_bunko.server import create_app, shutdown_app

    db = Database(base / "mokuro.db")
    for name, role in (("reader", "registered"), ("uploader", "uploader"), ("editor", "editor"), ("admin", "admin"), ("uploader2", "uploader")):
        db.create_user(name, "pass1234", role)
    config = Config(server=ServerConfig(host="127.0.0.1", port=port), storage=StorageConfig(base_path=base))
    app = create_app(config)
    # The metadata compiler writes `<Series>/series.json` / `catalog.json` into the library
    # on its own (20 s after start, and debounced after every change the watcher sees);
    # the in-process Rust handler has no such service, so the trees would diverge. Both
    # `stop()`s are permanent.
    app._library_watcher.stop()  # type: ignore[attr-defined]
    app._metadata_service.stop()  # type: ignore[attr-defined]
    server = Server(("127.0.0.1", port), app, numthreads=8)
    try:
        server.start()
    finally:
        shutdown_app(app)


if __name__ == "__main__":
    main()
