"""The PUT interception middleware (contract §6)."""

from __future__ import annotations

import io
import json
import zipfile
from pathlib import Path
from typing import Any

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.middleware import MAX_UPDATE_BODY_BYTES, MetadataAPI
from mokuro_bunko.metadata.service import MetadataService


class StubService:
    def __init__(self, accepted: bool = True) -> None:
        self.accepted = accepted
        self.calls: list[tuple[str, bytes, str | None]] = []

    def apply_series_update(
        self, series_title: str, payload: bytes, actor: str | None
    ) -> bool:
        self.calls.append((series_title, payload, actor))
        return self.accepted


class StubApp:
    def __init__(self) -> None:
        self.calls = 0

    def __call__(self, environ: dict[str, Any], start_response: Any) -> list[bytes]:
        self.calls += 1
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"downstream"]


def call(
    middleware: MetadataAPI,
    *,
    method: str = "PUT",
    path: str = "/mokuro-reader/Dr Stone/series.json",
    body: bytes = b'{"version":2}',
    username: str | None = "alice",
    content_length: str | None = None,
) -> tuple[str, list[tuple[str, str]], bytes]:
    captured: dict[str, Any] = {}

    def start_response(status: str, headers: list[tuple[str, str]]) -> None:
        captured["status"] = status
        captured["headers"] = headers

    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "CONTENT_LENGTH": str(len(body)) if content_length is None else content_length,
        "wsgi.input": io.BytesIO(body),
        "mokuro.username": username,
        "mokuro.user": {"username": username} if username else None,
    }
    result = b"".join(middleware(environ, start_response))
    return captured["status"], captured["headers"], result


class TestPassthrough:
    def test_non_put_requests_pass_through(self) -> None:
        downstream = StubApp()
        service = StubService()
        status, _headers, body = call(
            MetadataAPI(downstream, service=service), method="GET"  # type: ignore[arg-type]
        )
        assert status == "200 OK"
        assert body == b"downstream"
        assert downstream.calls == 1
        assert service.calls == []

    def test_puts_to_other_paths_pass_through(self) -> None:
        downstream = StubApp()
        status, _headers, _body = call(
            MetadataAPI(downstream, service=StubService()),  # type: ignore[arg-type]
            path="/mokuro-reader/Dr Stone/Volume 01.cbz",
        )
        assert status == "200 OK"
        assert downstream.calls == 1

    def test_a_nested_series_json_is_not_intercepted(self) -> None:
        downstream = StubApp()
        call(
            MetadataAPI(downstream, service=StubService()),  # type: ignore[arg-type]
            path="/mokuro-reader/Dr Stone/extras/series.json",
        )
        assert downstream.calls == 1


class TestInterception:
    def test_accepted_update_answers_204_and_never_reaches_the_dav_app(self) -> None:
        downstream = StubApp()
        service = StubService()
        status, _headers, body = call(
            MetadataAPI(downstream, service=service)  # type: ignore[arg-type]
        )
        assert status.startswith("204")
        assert body == b""
        assert downstream.calls == 0
        assert service.calls == [("Dr Stone", b'{"version":2}', "alice")]

    def test_a_rejected_update_is_an_ordinary_400(self) -> None:
        service = StubService(accepted=False)
        status, headers, body = call(
            MetadataAPI(StubApp(), service=service)  # type: ignore[arg-type]
        )
        assert status.startswith("400")
        assert dict(headers)["Content-Type"].startswith("text/plain")
        assert b"metadata" in body.lower()

    def test_an_anonymous_put_is_401(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service), username=None  # type: ignore[arg-type]
        )
        assert status.startswith("401")
        assert service.calls == []

    def test_an_oversized_body_is_413_and_is_not_read(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            content_length=str(MAX_UPDATE_BODY_BYTES + 1),
        )
        assert status.startswith("413")
        assert service.calls == []

    def test_a_missing_content_length_is_400(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service), content_length="not a number"  # type: ignore[arg-type]
        )
        assert status.startswith("400")
        assert service.calls == []

    def test_without_a_service_the_write_is_refused_not_written(self) -> None:
        downstream = StubApp()
        status, _headers, _body = call(MetadataAPI(downstream, service=None))
        assert status.startswith("403")
        assert downstream.calls == 0


class TestOnPublishedIntegration:
    """Closes T9 review mutations M29/M31: nobody pinned that an accepted PUT,
    driven through the real service (not a stub), fires `on_published` exactly
    once — not zero (cache never invalidated) and not twice (double refresh)."""

    def test_an_accepted_put_fires_on_published_exactly_once(self, tmp_path: Path) -> None:
        library = tmp_path / "library"
        folder = library / "Dr Stone"
        folder.mkdir(parents=True)
        with zipfile.ZipFile(folder / "Volume 01.cbz", "w") as archive:
            archive.writestr("000.jpg", b"fake image bytes")
        (folder / "Volume 01.mokuro").write_text(
            json.dumps(
                {
                    "version": "0.2.2",
                    "title": "Dr Stone",
                    "title_uuid": "t-uuid",
                    "volume": "Volume 01",
                    "volume_uuid": "uuid-Volume 01",
                    "pages": [{"blocks": [{"lines": ["世界"]}]}],
                }
            ),
            encoding="utf-8",
        )

        publish_calls: list[int] = []
        service = MetadataService(
            library,
            Database(tmp_path / "test.db"),
            on_published=lambda: publish_calls.append(1),
        )
        middleware = MetadataAPI(StubApp(), service=service)  # type: ignore[arg-type]

        body = json.dumps(
            {
                "version": 2,
                "series_title": "Dr Stone",
                "external_ids": {"anilist": 98416},
                "titles": {},
                "synonyms": [],
                "updated_at": "2026-08-18T19:36:24.324Z",
            }
        ).encode("utf-8")

        status, _headers, response_body = call(middleware, body=body)

        assert status.startswith("204")
        assert response_body == b""
        assert publish_calls == [1]

        sidecar = json.loads((folder / "series.json").read_text("utf-8"))
        assert sidecar["external_ids"] == {"anilist": 98416}
