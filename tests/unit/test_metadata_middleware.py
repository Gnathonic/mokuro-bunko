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


class _SpyInput(io.BytesIO):
    """A `wsgi.input` stand-in that records whether `.read()` was ever called.

    F2 regression: a rejected-before-read PUT (oversized, or missing
    Content-Length) must never touch the body at all.
    """

    def __init__(self, data: bytes) -> None:
        super().__init__(data)
        self.read_calls = 0

    def read(self, *args: Any, **kwargs: Any) -> bytes:
        self.read_calls += 1
        return super().read(*args, **kwargs)


def call(
    middleware: MetadataAPI,
    *,
    method: str = "PUT",
    path: str = "/mokuro-reader/Dr Stone/series.json",
    body: bytes = b'{"version":2}',
    username: str | None = "alice",
    content_length: str | None = None,
    omit_content_length: bool = False,
    wsgi_input: _SpyInput | None = None,
) -> tuple[str, list[tuple[str, str]], bytes]:
    captured: dict[str, Any] = {}

    def start_response(status: str, headers: list[tuple[str, str]]) -> None:
        captured["status"] = status
        captured["headers"] = headers

    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "wsgi.input": wsgi_input if wsgi_input is not None else _SpyInput(body),
        "mokuro.username": username,
        "mokuro.user": {"username": username} if username else None,
    }
    if not omit_content_length:
        environ["CONTENT_LENGTH"] = str(len(body)) if content_length is None else content_length
    result = b"".join(middleware(environ, start_response))
    return captured["status"], captured["headers"], result


class TestWsgiPathEncoding:
    """PEP 3333 delivers PATH_INFO as request bytes decoded latin-1; the DAV
    app below re-encodes for itself (wsgidav's `re_encode_path_info` hotfix)
    but this middleware sits ABOVE it and used to parse the mojibake — every
    non-ASCII series title failed folder resolution and was refused (102
    rejections in the first live upload session, `Ranma ½` retried 13×)."""

    def test_a_wsgi_encoded_utf8_path_is_intercepted_with_the_real_title(self) -> None:
        service = StubService()
        wsgi_path = "/mokuro-reader/ベルセルク/series.json".encode().decode("iso-8859-1")
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            path=wsgi_path,
        )
        assert status == "204 No Content"
        assert service.calls[0][0] == "ベルセルク"

    def test_an_already_unicode_path_is_used_verbatim(self) -> None:
        # A test harness (or a server that decoded for us) hands real unicode;
        # the latin-1 round-trip is impossible there and must be a no-op.
        service = StubService()
        call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            path="/mokuro-reader/ベルセルク/series.json",
        )
        assert service.calls[0][0] == "ベルセルク"

    def test_the_audit_path_is_the_decoded_spelling(self) -> None:
        class _SpyDb:
            def __init__(self) -> None:
                self.events: list[dict[str, Any]] = []

            def log_audit_event(self, **kwargs: Any) -> None:
                self.events.append(kwargs)

        db = _SpyDb()
        middleware = MetadataAPI(StubApp(), service=StubService())  # type: ignore[arg-type]
        wsgi_path = "/mokuro-reader/ベルセルク/series.json".encode().decode("iso-8859-1")
        body = b'{"version":2}'

        def start_response(status: str, headers: list[tuple[str, str]]) -> None:
            pass

        environ: dict[str, Any] = {
            "REQUEST_METHOD": "PUT",
            "PATH_INFO": wsgi_path,
            "CONTENT_LENGTH": str(len(body)),
            "wsgi.input": io.BytesIO(body),
            "mokuro.username": "alice",
            "mokuro.db": db,
        }
        b"".join(middleware(environ, start_response))
        assert len(db.events) == 1
        assert db.events[0]["target_path"] == "/mokuro-reader/ベルセルク/series.json"


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
        spy = _SpyInput(b"x" * (MAX_UPDATE_BODY_BYTES + 1))
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            content_length=str(MAX_UPDATE_BODY_BYTES + 1),
            wsgi_input=spy,
        )
        assert status.startswith("413")
        assert service.calls == []
        assert spy.read_calls == 0

    def test_a_missing_content_length_is_400(self) -> None:
        service = StubService()
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service), content_length="not a number"  # type: ignore[arg-type]
        )
        assert status.startswith("400")
        assert service.calls == []

    def test_an_absent_content_length_header_is_411_and_is_not_read(self) -> None:
        """F2: a chunked-style PUT (`Transfer-Encoding: chunked`, no
        `Content-Length` at all) must be rejected outright — never coerced
        by `int(None or 0)` into a phantom zero-length body that silently
        applies an empty update. 411 Length Required (RFC 9110 §15.5.12) is
        the standards-precise status for "I refuse this request without a
        declared length", and is kept distinct from the 400 just above
        (header PRESENT but garbage) so the two failure classes never blur.
        The body must never be touched either: on a real keep-alive cheroot
        connection, `ChunkedRFile` is the only thing that knows how to
        decode the chunked framing, and cheroot's own post-response drain
        step explicitly skips chunked requests (`chunked_read`) — reading
        a bounded/wrong amount here would desync the connection worse, not
        better, so the correct move is to answer without reading at all.
        """
        service = StubService()
        spy = _SpyInput(b'{"version":2}')
        status, _headers, _body = call(
            MetadataAPI(StubApp(), service=service),  # type: ignore[arg-type]
            omit_content_length=True,
            wsgi_input=spy,
        )
        assert status.startswith("411")
        assert service.calls == []
        assert spy.read_calls == 0

    def test_without_a_service_the_write_is_refused_not_written(self) -> None:
        downstream = StubApp()
        status, _headers, _body = call(MetadataAPI(downstream, service=None))
        assert status.startswith("403")
        assert downstream.calls == 0


class TestPathAliasInterception:
    """F1 regression: a legal alias spelling of `<Series>/series.json` — one
    the real path resolver (`security.safe_resolve_under` ->
    `Path.resolve()`) also lands on the same compiled file — must be
    intercepted exactly like the canonical spelling: never reach the DAV
    app, and go through the service like any other accepted update."""

    ALIAS_PATHS = [
        "/mokuro-reader/Dr Stone//series.json",
        "/mokuro-reader/Dr Stone/./series.json",
        "/mokuro-reader/./Dr Stone/series.json",
        "/mokuro-reader/Dr Stone/../Dr Stone/series.json",
    ]

    def test_alias_spellings_are_intercepted_not_passed_through(self) -> None:
        for path in self.ALIAS_PATHS:
            downstream = StubApp()
            service = StubService()
            status, _headers, _body = call(
                MetadataAPI(downstream, service=service),  # type: ignore[arg-type]
                path=path,
            )
            assert downstream.calls == 0, f"{path!r} reached the DAV app"
            assert status.startswith("204"), f"{path!r} was not accepted: {status}"
            assert service.calls == [("Dr Stone", b'{"version":2}', "alice")], path

    def test_a_traversal_escaping_the_library_root_is_never_intercepted(self) -> None:
        """Not a bypass: `safe_resolve_under` independently refuses to
        resolve this outside the library root either, so wsgidav never
        opens a writer for it — it simply isn't a `series.json` alias at
        all, and the matcher must agree by not intercepting it."""
        downstream = StubApp()
        status, _headers, _body = call(
            MetadataAPI(downstream, service=StubService()),  # type: ignore[arg-type]
            path="/mokuro-reader/../etc/series.json",
        )
        assert status == "200 OK"
        assert downstream.calls == 1


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
