"""Tests for server._validate_startup_environment."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.config import Config, SslConfig, StorageConfig
from mokuro_bunko.server import _validate_startup_environment
from mokuro_bunko.ssl import generate_self_signed_cert


def _config(tmp: Path, ssl: SslConfig | None = None) -> Config:
    return Config(
        storage=StorageConfig(base_path=tmp / "storage"),
        ssl=ssl or SslConfig(enabled=False),
    )


def test_passes_with_ssl_disabled(tmp_path: Path) -> None:
    # Should create + verify storage dirs and return without raising.
    _validate_startup_environment(_config(tmp_path))


def test_ssl_enabled_missing_cert_raises(tmp_path: Path) -> None:
    ssl = SslConfig(
        enabled=True,
        cert_file=str(tmp_path / "missing.crt"),
        key_file=str(tmp_path / "missing.key"),
    )
    with pytest.raises(ValueError):
        _validate_startup_environment(_config(tmp_path, ssl))


def test_ssl_enabled_valid_cert_passes(tmp_path: Path) -> None:
    cert = tmp_path / "s.crt"
    key = tmp_path / "s.key"
    generate_self_signed_cert(cert, key, hostname="localhost", validity_days=365)
    ssl = SslConfig(enabled=True, cert_file=str(cert), key_file=str(key))
    _validate_startup_environment(_config(tmp_path, ssl))


def test_ssl_enabled_mismatched_cert_raises(tmp_path: Path) -> None:
    # Both files exist but don't match -> validate_certificate_pair errors -> raise.
    cert_a = tmp_path / "a.crt"
    key_a = tmp_path / "a.key"
    cert_b = tmp_path / "b.crt"
    key_b = tmp_path / "b.key"
    generate_self_signed_cert(cert_a, key_a, hostname="localhost", validity_days=365)
    generate_self_signed_cert(cert_b, key_b, hostname="localhost", validity_days=365)
    ssl = SslConfig(enabled=True, cert_file=str(cert_a), key_file=str(key_b))
    with pytest.raises(ValueError):
        _validate_startup_environment(_config(tmp_path, ssl))


def test_ssl_auto_cert_fresh_dir_passes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # First run with auto_cert: the certs dir doesn't exist yet. Validation
    # must create it (generate_self_signed_cert would) rather than fail.
    fresh_data_home = tmp_path / "data-home"  # deliberately not created
    monkeypatch.setenv("XDG_DATA_HOME", str(fresh_data_home))
    ssl = SslConfig(enabled=True, auto_cert=True)
    _validate_startup_environment(_config(tmp_path, ssl))
