"""Layer sidecars across the read-side surfaces: library index, queue API,
WebDAV delete cascade.

The three surfaces answer three different questions about the same files and
must not be confused: the index says what is ON DISK, the queue says what is
CONFIGURED and still missing, and the delete cascade says what BELONGS to an
archive. None of them may be derived from either of the others -- a
generation can be renamed, disabled or deleted while its files stay, and
another server or a reader may have written a layer this one never heard of.
"""

from __future__ import annotations

import io
import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.webdav.provider import MokuroDAVProvider
from mokuro_bunko.webdav.resources import MokuroFileResource

MOKURO_ROW = {"name": "mokuro", "engine": "mokuro", "primary": True}


def _generations(*extra: dict[str, Any]) -> list[GenerationSpec]:
    """A generations list: the primary mokuro row plus whatever is asked."""
    return parse_generation_list([dict(MOKURO_ROW), *extra])


class _Resp:
    def __init__(self) -> None:
        self.status = ""
        self.content = b""

    def start_response(
        self, status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        self.status = status
        return lambda data: None


def _get(app: Callable[..., Any], path: str) -> dict[str, Any]:
    resp = _Resp()
    environ = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.url_scheme": "http",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
    }
    b"".join(app(environ, resp.start_response))
    assert resp.status.startswith("200"), resp.status
    # The endpoint sends a shaped, per-level payload (`queue.shape`); these
    # tests are about the model underneath it, which `raw_status` returns.
    return app.raw_status()  # type: ignore[attr-defined, no-any-return]


def _dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [])
    return [b""]


class TestLibraryIndex:
    def test_layer_sidecars_indexed_without_phantom_volumes(self, tmp_path: Path) -> None:
        library = tmp_path / "library"
        series = library / "Series"
        series.mkdir(parents=True)
        (series / "Vol 01.cbz").write_bytes(b"cbz")
        (series / "Vol 01.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 01.hayai-nova.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 02.cbz").write_bytes(b"cbz")
        (series / "Vol 02.paddle-manga.mokuro.gz").write_bytes(b"gz")

        snapshot = LibraryIndexCache(library, ttl=60.0).get_snapshot()
        vols = {v.name: v for v in snapshot.series[0].volumes}
        assert set(vols) == {"Vol 01", "Vol 02"}  # no "Vol 01.hayai-nova" phantom
        assert vols["Vol 01"].has_mokuro
        assert vols["Vol 01"].sidecars == ("hayai-nova",)
        assert not vols["Vol 02"].has_mokuro
        assert vols["Vol 02"].sidecars == ("paddle-manga",)
        # A layer sidecar never satisfies the reader-facing mokuro pending flag.
        assert snapshot.pending_ocr == (("Series", "Vol 02"),)

    def test_a_layer_no_generation_writes_is_still_indexed(self, tmp_path: Path) -> None:
        """The index reports what is THERE, not what is configured.

        ``hayai`` (the withdrawn v2 engine) no longer runs and no generation
        is named after it, but its files are in real libraries -- as are
        layers another server wrote and edits a reader pushed. Stop
        recognizing ``.hayai.mokuro`` and every one of them shows up as a
        volume called ``Vol 01.hayai`` -- with no cbz, so also as pending OCR
        forever.
        """
        library = tmp_path / "library"
        series = library / "Series"
        series.mkdir(parents=True)
        (series / "Vol 01.cbz").write_bytes(b"cbz")
        (series / "Vol 01.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 01.hayai.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 01.tr-en.mokuro").write_text("{}", encoding="utf-8")
        # Not a postfix any reader parses as a layer id, so not a layer here.
        (series / "Vol 01.backup.v1.mokuro").write_text("{}", encoding="utf-8")

        snapshot = LibraryIndexCache(library, ttl=60.0).get_snapshot()
        vols = {v.name: v for v in snapshot.series[0].volumes}
        assert set(vols) == {"Vol 01"}
        assert vols["Vol 01"].sidecars == ("hayai", "tr-en")
        assert snapshot.pending_ocr == ()


class TestQueueApi:
    def test_status_lists_pending_per_generation_and_tags_failures(self, tmp_path: Path) -> None:
        storage = tmp_path
        series = storage / "library" / "S"
        series.mkdir(parents=True)
        (series / "A.cbz").write_bytes(b"cbz")
        (series / "A.mokuro").write_text("{}", encoding="utf-8")
        (series / "B.cbz").write_bytes(b"cbz")
        (series / "B.mokuro").write_text("{}", encoding="utf-8")
        (series / "B.hayai-nova.mokuro").write_text("{}", encoding="utf-8")
        # C owes the primary row its file; its other rows are owed all the
        # same (no layer waits for the primary).
        (series / "C.cbz").write_bytes(b"cbz")
        # The primary row keeps the bare relative path as its failure key;
        # every other row is suffixed with its NAME (`OCRWorker.failure_key`).
        (storage / ".ocr-failures.json").write_text(
            json.dumps(
                {
                    "S/C.cbz": {
                        "series": "S",
                        "volume": "C",
                        "generation": "mokuro",
                        "engine": "mokuro",
                        "detector": None,
                        "error": "boom",
                        "attempts": 1,
                        "last_attempt_at": time.time(),
                    },
                    "S/A.cbz@hayai-nova": {
                        "series": "S",
                        "volume": "A",
                        "generation": "hayai-nova",
                        "engine": "hayai-nova",
                        "detector": "ppocr-manga",
                        "error": "oom",
                        "attempts": 2,
                        "last_attempt_at": time.time(),
                    },
                }
            ),
            encoding="utf-8",
        )
        (storage / ".ocr-progress.json").write_text(
            json.dumps(
                {
                    "active": True,
                    "series": "S",
                    "volume": "A",
                    "generation": "paddle-manga",
                    "engine": "paddle-manga",
                    "detector": "ppocr-manga",
                    "percent": 40,
                    "status": "running",
                }
            ),
            encoding="utf-8",
        )

        app = QueueAPI(
            _dummy_app,
            storage_base_path=str(storage),
            generations=_generations(
                {"name": "hayai-nova", "engine": "hayai-nova"},
                {"name": "paddle-manga", "engine": "paddle-manga"},
            ),
        )
        data = _get(app, "/queue/api/status")

        assert [row["name"] for row in data["generations"]] == [
            "mokuro",
            "hayai-nova",
            "paddle-manga",
        ]
        assert data["current"]["generation"] == "paddle-manga"
        pending = [(p["series"], p["volume"], p["generation"]) for p in data["pending_ocr"]]
        # C/mokuro and A/hayai-nova are in the failed list, so they leave
        # pending; A/paddle-manga is the running job (`current`) and is not
        # listed twice. What is left is C's other rows and B's last one (its
        # primary and hayai-nova files are already there), row by row.
        assert pending == [
            ("S", "C", "hayai-nova"), ("S", "B", "paddle-manga"), ("S", "C", "paddle-manga"),
        ]
        failed = {(f["volume"], f["generation"], f["engine"]) for f in data["failed"]}
        assert failed == {("C", "mokuro", "mokuro"), ("A", "hayai-nova", "hayai-nova")}

    def test_row_order_decides_the_queue(self, tmp_path: Path) -> None:
        """The list order IS the priority, and any engine may be primary.

        There is no speed ranking left to second-guess it: a row running
        mokuro in fp16 as the primary one writes the bare sidecar (so that
        file, not a `.fp16.mokuro`, is what settles its job), and moving a
        row up the list moves its jobs up the queue.
        """
        series = tmp_path / "library" / "S"
        series.mkdir(parents=True)
        (series / "A.cbz").write_bytes(b"cbz")
        (series / "A.mokuro").write_text("{}", encoding="utf-8")
        rows = parse_generation_list(
            [
                {"name": "fp16", "engine": "mokuro", "primary": True, "precision": "fp16"},
                {"name": "nova", "engine": "hayai-nova"},
                {"name": "paddle", "engine": "paddle-manga"},
            ]
        )
        app = QueueAPI(_dummy_app, storage_base_path=str(tmp_path), generations=rows)
        data = _get(app, "/queue/api/status")
        # The fp16 row is satisfied by the bare file it writes, and is not
        # listed as owing a `.fp16.mokuro` of its own.
        assert [(p["volume"], p["generation"], p["engine"]) for p in data["pending_ocr"]] == [
            ("A", "nova", "hayai-nova"),
            ("A", "paddle", "paddle-manga"),
        ]

        app.generations = [rows[0], rows[2], rows[1]]
        data = _get(app, "/queue/api/status")
        assert [p["generation"] for p in data["pending_ocr"]] == ["paddle", "nova"]

    def test_generations_can_change_live(self, tmp_path: Path) -> None:
        series = tmp_path / "library" / "S"
        series.mkdir(parents=True)
        (series / "A.cbz").write_bytes(b"cbz")
        (series / "A.mokuro").write_text("{}", encoding="utf-8")
        app = QueueAPI(
            _dummy_app,
            storage_base_path=str(tmp_path),
            generations=_generations(
                {"name": "nova", "engine": "hayai-nova"},
                {"name": "paddle", "engine": "paddle-manga"},
            ),
        )
        assert len(_get(app, "/queue/api/status")["pending_ocr"]) == 2
        app.generations = _generations({"name": "nova", "engine": "hayai-nova"})
        data = _get(app, "/queue/api/status")
        assert [row["name"] for row in data["generations"]] == ["mokuro", "nova"]
        # Only the job's identity: the entries also carry a prediction, and
        # against a server with no OCR worker every part of it is null.
        assert [
            {key: row[key] for key in ("series", "volume", "generation", "engine", "detector")}
            for row in data["pending_ocr"]
        ] == [
            {
                "series": "S",
                "volume": "A",
                "generation": "nova",
                "engine": "hayai-nova",
                "detector": "ppocr-manga",
            }
        ]

    def test_default_list_is_one_mokuro_row(self, tmp_path: Path) -> None:
        (tmp_path / "library" / "S").mkdir(parents=True)
        (tmp_path / "library" / "S" / "A.cbz").write_bytes(b"cbz")
        data = _get(QueueAPI(_dummy_app, storage_base_path=str(tmp_path)), "/queue/api/status")
        assert data["generations"] == [
            {"id": "g-1", "name": "mokuro", "engine": "mokuro", "detector": None}
        ]
        assert [
            {key: row[key] for key in ("series", "volume", "generation", "engine", "detector")}
            for row in data["pending_ocr"]
        ] == [
            {
                "series": "S",
                "volume": "A",
                "generation": "mokuro",
                "engine": "mokuro",
                "detector": None,
            }
        ]
        # No OCR worker: nothing has measured a rate, so nothing is promised.
        assert data["pending_ocr"][0]["eta_at"] is None
        assert data["queue_done_at"] is None


class TestWebdavDeleteCascade:
    """Deleting an archive takes every file that belongs to it.

    Which files those are is decided by LISTING the directory and matching
    the reader's layer grammar, not by a registry of configured names: the
    server that deletes a volume may never have run the generation that
    wrote a layer beside it.
    """

    def _resource(self, storage: Path, physical: Path) -> MokuroFileResource:
        provider = MokuroDAVProvider(storage)
        environ: dict[str, object] = {"wsgidav.provider": provider}
        virtual = f"/mokuro-reader/{physical.parent.name}/{physical.name}"
        return MokuroFileResource(virtual, environ, physical)

    def test_deleting_the_archive_sweeps_every_layer_beside_it(self, tmp_path: Path) -> None:
        series = tmp_path / "library" / "S"
        series.mkdir(parents=True)
        cbz = series / "Vol 01.cbz"
        cbz.write_bytes(b"cbz")
        swept = [
            "Vol 01.mokuro",
            "Vol 01.hayai-nova.mokuro",
            "Vol 01.paddle-manga.mokuro.gz",
            # A generation name nothing here is configured for, and a layer
            # a reader pushed: both are this volume's files.
            "Vol 01.my-own-name.mokuro",
            "Vol 01.tr-en.mokuro",
            "Vol 01.webp",
            "Vol 01.nocover",
        ]
        kept = ["Vol 01.backup.v1.mokuro", "Vol 02.mokuro", "Vol 02.cbz"]
        for name in [*swept, *kept]:
            (series / name).write_text("{}", encoding="utf-8")

        self._resource(tmp_path, cbz).delete()

        assert not cbz.exists()
        assert [name for name in swept if (series / name).exists()] == []
        assert [name for name in kept if not (series / name).exists()] == []

    def test_deleting_a_sidecar_takes_nothing_with_it(self, tmp_path: Path) -> None:
        """Only the ARCHIVE cascades; a layer is deleted on its own.

        A reader replacing one layer must not take the volume's other files
        (or the volume) with it.
        """
        series = tmp_path / "library" / "S"
        series.mkdir(parents=True)
        cbz = series / "Vol 01.cbz"
        cbz.write_bytes(b"cbz")
        layer = series / "Vol 01.hayai-nova.mokuro"
        layer.write_text("{}", encoding="utf-8")
        (series / "Vol 01.mokuro").write_text("{}", encoding="utf-8")

        self._resource(tmp_path, layer).delete()

        assert not layer.exists()
        assert cbz.exists()
        assert (series / "Vol 01.mokuro").exists()
