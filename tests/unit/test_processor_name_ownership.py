"""A machine's name belongs to the processor account that first used it.

Names used to be de-duplicated within one account only, so a second
processor account registering as ``tower`` overwrote tower's identity and
received the pools the admin had saved for tower. And whatever a
registration sent as ``host`` and ``catalog`` (up to the 256 KB body cap)
was written to disk under every new name it tried.
"""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles, profiles_dir
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry


def _api(tmp_path: Path) -> ProcessorAPI:
    return ProcessorAPI(
        lambda e, s: [], ProcessorRegistry(), profiles=ProcessorProfiles(tmp_path)
    )


def _register(
    api: ProcessorAPI, username: str, name: str, **extra: Any
) -> tuple[str, dict[str, Any]]:
    body = json.dumps(
        {"protocol": PROTOCOL_VERSION, "name": name, "catalog": {}, **extra}
    ).encode()
    environ = {
        "REQUEST_METHOD": "POST", "PATH_INFO": "/_processor/register",
        "CONTENT_LENGTH": str(len(body)), "wsgi.input": io.BytesIO(body),
        "mokuro.role": "processor", "mokuro.username": username,
    }
    out: list[str] = []
    raw = b"".join(api(environ, lambda status, headers: out.append(status)))
    return out[0], dict(json.loads(raw))


def test_another_account_cannot_take_a_name(tmp_path: Path) -> None:
    api = _api(tmp_path)
    assert _register(api, "acct-a", "tower", host={"gpu": "RTX 4090"})[0].startswith("200")

    status, body = _register(api, "acct-b", "tower", host={"gpu": "impostor"})

    assert status.startswith("409")
    assert "another processor account" in body["error"]
    assert ProcessorProfiles(tmp_path).load("tower")["host"] == {"gpu": "RTX 4090"}


def test_the_owner_reconnects_under_its_own_name(tmp_path: Path) -> None:
    api = _api(tmp_path)
    assert _register(api, "acct-a", "tower")[0].startswith("200")
    assert _register(api, "acct-a", "tower")[0].startswith("200")


def test_a_profile_from_before_ownership_is_claimed_by_its_next_registration(
    tmp_path: Path,
) -> None:
    ProcessorProfiles(tmp_path).set_pools("tower", "g-1", {"engine": 2})
    api = _api(tmp_path)
    assert _register(api, "acct-a", "tower")[0].startswith("200")
    assert _register(api, "acct-b", "tower")[0].startswith("409")


def test_an_oversized_identity_is_refused_and_not_written(tmp_path: Path) -> None:
    api = _api(tmp_path)
    status, body = _register(api, "acct-a", "tower", host={"pad": "x" * 64 * 1024})

    assert status.startswith("413")
    assert "host" in body["error"]
    assert not list(profiles_dir(tmp_path).glob("*.json"))


def test_return_classes_a_processor_invents_are_bounded() -> None:
    from mokuro_bunko.ocr.remote.registry import MAX_RETURN_CLASSES, TransferStats

    stats = TransferStats()
    for n in range(1000):
        stats.note_returned({"class": f"made-up-{n}"})
    assert len(stats.returned) <= MAX_RETURN_CLASSES
    assert sum(stats.returned.values()) == 1000
