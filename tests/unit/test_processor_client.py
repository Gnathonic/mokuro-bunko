"""The processor's config and its client. (How it reads an archive: test_processor_archives.)"""

from __future__ import annotations

import json
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.processor.config import ProcessorConfigError, load_processor_config


def _write(path: Path, body: str) -> Path:
    path.write_text(body, encoding="utf-8")
    return path


class TestTheConfig:
    def test_a_minimal_file_loads_with_sensible_defaults(self, tmp_path: Path) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n"
            "  url: https://library.example:8080\n"
            "  username: tower\n"
            "  password: hunter2hunter2\n",
        )
        config = load_processor_config(path)
        assert config.library.url == "https://library.example:8080"
        assert config.library.tls_verify is True
        assert config.processor.max_sessions == 1
        assert config.processor.name
        assert config.ocr.backend == "auto"

    def test_every_documented_key_is_accepted(self, tmp_path: Path) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n"
            "  url: https://library.example:8080\n"
            "  username: tower\n"
            "  password: hunter2hunter2\n"
            "  tls_verify: /etc/ssl/library.pem\n"
            "processor:\n"
            "  name: tower\n"
            "  max_sessions: 2\n"
            f"  storage: {tmp_path / 'state'}\n"
            "ocr:\n"
            "  backend: cuda\n",
        )
        config = load_processor_config(path)
        assert config.library.tls_verify == "/etc/ssl/library.pem"
        assert config.processor.name == "tower"
        assert config.processor.max_sessions == 2
        assert config.processor.storage == tmp_path / "state"
        assert config.ocr.backend == "cuda"

    def test_a_missing_credential_is_named(self, tmp_path: Path) -> None:
        path = _write(tmp_path / "processor.yaml", "library:\n  url: https://x:8080\n")
        with pytest.raises(ProcessorConfigError) as excinfo:
            load_processor_config(path)
        assert "library.username" in str(excinfo.value)

    def test_an_unknown_key_is_refused_rather_than_ignored(self, tmp_path: Path) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n"
            "  url: https://x:8080\n"
            "  username: tower\n"
            "  password: hunter2hunter2\n"
            "  verify_tls: true\n",
        )
        with pytest.raises(ProcessorConfigError) as excinfo:
            load_processor_config(path)
        assert "verify_tls" in str(excinfo.value)

    def test_the_archive_memory_budget_defaults_to_two_gigabytes(
        self, tmp_path: Path
    ) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n  url: https://x:8080\n  username: b\n  password: hunter2hunter2\n",
        )
        assert load_processor_config(path).processor.archive_memory_mb == 2048

    def test_an_archive_memory_budget_of_zero_means_disk_and_is_accepted(
        self, tmp_path: Path
    ) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n  url: https://x:8080\n  username: b\n  password: hunter2hunter2\n"
            "processor:\n  archive_memory_mb: 0\n",
        )
        assert load_processor_config(path).processor.archive_memory_mb == 0

    @pytest.mark.parametrize("value", ["-1", "lots", "true", "1.5"])
    def test_a_bad_archive_memory_budget_is_named(self, tmp_path: Path, value: str) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n  url: https://x:8080\n  username: b\n  password: hunter2hunter2\n"
            f"processor:\n  archive_memory_mb: {value}\n",
        )
        with pytest.raises(ProcessorConfigError) as excinfo:
            load_processor_config(path)
        assert "processor.archive_memory_mb" in str(excinfo.value)

    def test_a_bad_backend_is_named(self, tmp_path: Path) -> None:
        path = _write(
            tmp_path / "processor.yaml",
            "library:\n  url: https://x:8080\n  username: b\n  password: hunter2hunter2\n"
            "ocr:\n  backend: opencl\n",
        )
        with pytest.raises(ProcessorConfigError) as excinfo:
            load_processor_config(path)
        assert "opencl" in str(excinfo.value)


@pytest.fixture
def server(tmp_path: Path) -> Iterator[dict[str, Any]]:
    """A real HTTP server on a throwaway port, not a mock."""
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    state: dict[str, Any] = {"registered": []}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args: Any) -> None:
            return

        def do_POST(self) -> None:  # noqa: N802
            if self.path == "/_processor/register":
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length))
                state["registered"].append((self.headers.get("Authorization"), body))
                payload = json.dumps({
                    "protocol": PROTOCOL_VERSION, "processor_id": "p1",
                    "session_stream": "/_processor/p1/stream",
                    "events": "/_processor/p1/sessions/{sid}/events",
                    "archives": "/mokuro-reader/",
                }).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            self.send_response(404)
            self.end_headers()

        def do_GET(self) -> None:  # noqa: N802
            if self.path.endswith("/stream"):
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.end_headers()
                for op in ({"op": "heartbeat"}, {"op": "close_session", "sid": "s1"}):
                    self.wfile.write((json.dumps(op) + "\n").encode())
                    self.wfile.flush()
                return
            self.send_response(404)
            self.end_headers()

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    state["port"] = httpd.server_address[1]
    yield state
    httpd.shutdown()


def _client(server: dict[str, Any], tmp_path: Path) -> Any:
    from mokuro_bunko.processor.client import LibraryClient
    from mokuro_bunko.processor.config import (
        LibrarySettings,
        ProcessorConfig,
        ProcessorOcr,
        ProcessorSettings,
    )

    return LibraryClient(
        ProcessorConfig(
            library=LibrarySettings(
                url=f"http://127.0.0.1:{server['port']}",
                username="tower",
                password="hunter2hunter2",
            ),
            processor=ProcessorSettings(name="tower", storage=tmp_path / "state"),
            ocr=ProcessorOcr(),
        )
    )


class TestTheClient:
    def test_register_sends_basic_auth_and_the_catalog(
        self, server: dict[str, Any], tmp_path: Path
    ) -> None:
        client = _client(server, tmp_path)
        reply = client.register(
            {"engines": ["hayai-nova"], "detectors": ["ctd"], "devices": []},
            {"gpu": "RTX 4090"},
        )
        assert reply["processor_id"] == "p1"
        header, body = server["registered"][0]
        assert header.startswith("Basic ")
        assert body["protocol"] == PROTOCOL_VERSION
        assert body["name"] == "tower"
        assert body["catalog"]["engines"] == ["hayai-nova"]
        client.close()

    def test_ops_arrive_as_dicts_in_order(
        self, server: dict[str, Any], tmp_path: Path
    ) -> None:
        client = _client(server, tmp_path)
        client.register({"engines": [], "detectors": [], "devices": []}, {})
        assert [op["op"] for op in client.ops()] == ["heartbeat", "close_session"]
        client.close()


class TestTheCatalog:
    """What a processor tells the library it can run."""

    def test_every_installed_detector_is_reported_not_only_the_default(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A row names its detector, and the library offers a processor only
        the rows whose detector it reported (`catalog_can_run`). Found on
        real hardware: the catalog asked a DEFAULT `EnginesInstaller()`,
        whose detector list was just the default one, so a 4090 with `ctd`
        and `ppocr-manga` installed was never offered a `ctd` row -- or the
        `ppocr-manga` engine it listed as installed."""
        from mokuro_bunko.ocr import bench, installer
        from mokuro_bunko.processor.cli import _catalog

        installed = {"ctd", "ppocr-manga"}
        monkeypatch.delenv("MOKURO_PROCESSOR_RUNNER", raising=False)
        monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", str(tmp_path / "python"))
        monkeypatch.setattr(installer.EnginesInstaller, "is_installed", lambda self: True)
        monkeypatch.setattr(
            installer.EnginesInstaller,
            "has_detector",
            lambda self, detector=None: (
                all(d in installed for d in self.detectors)
                if detector is None
                else detector in installed
            ),
        )
        monkeypatch.setattr(installer.OCRInstaller, "is_installed", lambda self: False)

        class _Devices:
            ort_gpu_providers = None  # the probe could not ask onnxruntime

            def entries(self) -> list[dict[str, Any]]:
                return [{"id": "auto", "label": "Auto"}]

        monkeypatch.setattr(bench, "probe_devices", lambda python: _Devices())
        monkeypatch.setattr(bench, "describe_host", lambda backend, python: {"gpu": None})
        config = load_processor_config(
            _write(
                tmp_path / "processor.yaml",
                "library:\n"
                "  url: https://library.example:8080\n"
                "  username: tower\n"
                "  password: hunter2hunter2\n",
            )
        )

        catalog, _host = _catalog(config)

        assert catalog["detectors"] == ["ctd", "ppocr-manga"]
