"""SSL support for mokuro-bunko.

Handles certificate generation and SSL context creation.
"""

from __future__ import annotations

import os
import ssl
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from mokuro_bunko.config import SslConfig


def get_default_cert_paths() -> tuple[Path, Path]:
    """Get default paths for SSL certificates.

    Returns:
        Tuple of (cert_path, key_path).
    """
    if os.name == "nt":  # Windows
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:  # Linux/macOS
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))

    cert_dir = base / "mokuro-bunko" / "certs"
    return cert_dir / "cert.pem", cert_dir / "key.pem"


def generate_self_signed_cert(
    cert_path: Path,
    key_path: Path,
    hostname: str = "localhost",
    validity_days: int = 365,
) -> None:
    """Generate a self-signed certificate.

    Args:
        cert_path: Path to save the certificate.
        key_path: Path to save the private key.
        hostname: Hostname for the certificate.
        validity_days: Number of days the certificate is valid.
    """
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.x509.oid import NameOID

    # Generate private key
    private_key = rsa.generate_private_key(
        public_exponent=65537,
        key_size=2048,
    )

    # Build certificate subject
    subject = issuer = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, hostname),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "mokuro-bunko"),
    ])

    # Build certificate
    now = datetime.now(timezone.utc)
    cert_builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(private_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now)
        .not_valid_after(now + timedelta(days=validity_days))
    )

    # Add subject alternative names
    san_list = [
        x509.DNSName("localhost"),
        x509.DNSName(hostname),
        x509.IPAddress(ipaddress_from_string("127.0.0.1")),
    ]

    # Try to add local hostname
    try:
        import socket
        local_hostname = socket.gethostname()
        if local_hostname and local_hostname != hostname:
            san_list.append(x509.DNSName(local_hostname))
    except Exception:
        pass

    cert_builder = cert_builder.add_extension(
        x509.SubjectAlternativeName(san_list),
        critical=False,
    )

    # Add basic constraints (not a CA)
    cert_builder = cert_builder.add_extension(
        x509.BasicConstraints(ca=False, path_length=None),
        critical=True,
    )

    # Sign the certificate
    certificate = cert_builder.sign(private_key, hashes.SHA256())

    # Ensure directories exist
    cert_path.parent.mkdir(parents=True, exist_ok=True)
    key_path.parent.mkdir(parents=True, exist_ok=True)

    # Write certificate
    with open(cert_path, "wb") as f:
        f.write(certificate.public_bytes(serialization.Encoding.PEM))

    # Write private key
    with open(key_path, "wb") as f:
        f.write(
            private_key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.TraditionalOpenSSL,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )


def ipaddress_from_string(addr: str) -> object:
    """Convert string IP address to x509.IPAddress.

    Args:
        addr: IP address string.

    Returns:
        IPAddress object for use in SAN.
    """
    import ipaddress
    return ipaddress.ip_address(addr)


def ensure_ssl_context(ssl_config: SslConfig) -> ssl.SSLContext | None:
    """Ensure SSL context is available based on configuration.

    Args:
        ssl_config: SSL configuration.

    Returns:
        SSL context if SSL is enabled, None otherwise.
    """
    if not ssl_config.enabled:
        return None

    if ssl_config.auto_cert:
        cert_path, key_path = get_default_cert_paths()

        # Generate cert if it doesn't exist
        if not cert_path.exists() or not key_path.exists():
            print(f"Generating self-signed certificate at {cert_path}")
            generate_self_signed_cert(cert_path, key_path)
    else:
        cert_path = Path(ssl_config.cert_file)
        key_path = Path(ssl_config.key_file)

        if not cert_path.exists():
            raise FileNotFoundError(f"Certificate file not found: {cert_path}")
        if not key_path.exists():
            raise FileNotFoundError(f"Key file not found: {key_path}")

    # Create SSL context
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(str(cert_path), str(key_path))

    return context


def get_ssl_info(ssl_config: SslConfig) -> str:
    """Get human-readable SSL configuration info.

    Args:
        ssl_config: SSL configuration.

    Returns:
        String describing SSL configuration.
    """
    if not ssl_config.enabled:
        return "SSL disabled"

    if ssl_config.auto_cert:
        cert_path, _ = get_default_cert_paths()
        return f"SSL enabled (auto-cert: {cert_path})"

    return f"SSL enabled (cert: {ssl_config.cert_file})"


def validate_certificate_pair(
    cert_path: Path,
    key_path: Path,
    *,
    expiry_warning_days: int = 30,
) -> tuple[list[str], list[str]]:
    """Validate certificate/key integrity and return (errors, warnings)."""
    errors: list[str] = []
    warnings: list[str] = []

    # Validate that certificate and key can be loaded together.
    try:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(str(cert_path), str(key_path))
    except Exception as exc:
        errors.append(f"SSL certificate/key validation failed: {exc}")
        return errors, warnings

    # Parse certificate dates for expiration checks.
    try:
        from cryptography import x509
    except Exception as exc:
        warnings.append(f"Could not import cryptography for certificate date checks: {exc}")
        return errors, warnings

    try:
        cert = x509.load_pem_x509_certificate(cert_path.read_bytes())
    except Exception as exc:
        errors.append(f"Failed to parse certificate file: {exc}")
        return errors, warnings

    now = datetime.now(timezone.utc)
    if cert.not_valid_after_utc <= now:
        errors.append(f"SSL certificate has expired: {cert.not_valid_after_utc.isoformat()}")
        return errors, warnings

    if cert.not_valid_before_utc > now:
        errors.append(f"SSL certificate is not valid yet: {cert.not_valid_before_utc.isoformat()}")
        return errors, warnings

    remaining = cert.not_valid_after_utc - now
    if remaining <= timedelta(days=expiry_warning_days):
        warnings.append(
            "SSL certificate expires soon "
            f"({cert.not_valid_after_utc.isoformat()}, {remaining.days} days remaining)"
        )

    return errors, warnings
