"""Tests for ssl.validate_certificate_pair."""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.ssl import generate_self_signed_cert, validate_certificate_pair


def _gen(tmp: Path, name: str = "srv", days: int = 365) -> tuple[Path, Path]:
    cert = tmp / f"{name}.crt"
    key = tmp / f"{name}.key"
    generate_self_signed_cert(cert, key, hostname="localhost", validity_days=days)
    return cert, key


def _write_expired_pair(cert_path: Path, key_path: Path) -> None:
    """Write a well-formed cert/key whose validity window is entirely in the past."""
    from datetime import datetime, timedelta, timezone

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.x509.oid import NameOID

    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    now = datetime.now(timezone.utc)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(days=10))
        .not_valid_after(now - timedelta(days=1))
        .sign(key, hashes.SHA256())
    )
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.TraditionalOpenSSL,
            serialization.NoEncryption(),
        )
    )


def test_valid_pair_has_no_errors(tmp_path: Path) -> None:
    cert, key = _gen(tmp_path)
    errors, warnings = validate_certificate_pair(cert, key)
    assert errors == []
    assert warnings == []  # 365-day cert is well beyond the 30-day warning window


def test_mismatched_cert_and_key_errors(tmp_path: Path) -> None:
    cert_a, _ = _gen(tmp_path, "a")
    _, key_b = _gen(tmp_path, "b")
    errors, _warnings = validate_certificate_pair(cert_a, key_b)
    assert errors  # cert/key don't match -> load_cert_chain fails


def test_expired_cert_errors(tmp_path: Path) -> None:
    cert = tmp_path / "exp.crt"
    key = tmp_path / "exp.key"
    _write_expired_pair(cert, key)
    errors, _warnings = validate_certificate_pair(cert, key)
    assert any("expired" in e.lower() for e in errors)


def test_cert_expiring_soon_warns(tmp_path: Path) -> None:
    cert, key = _gen(tmp_path, "soon", days=10)
    errors, warnings = validate_certificate_pair(cert, key)
    assert errors == []
    assert any("expires soon" in w.lower() for w in warnings)
