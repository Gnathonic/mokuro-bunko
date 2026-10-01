//! TLS: explicit PEM cert/key, or an auto-generated self-signed pair (0.5.2 `ssl.py`).
//!
//! Deviation: the auto certificate uses ECDSA P-256 instead of RSA-2048 (rcgen + ring
//! cannot generate RSA keys; every client that speaks TLS 1.2+ accepts P-256).

use bunko_core::config::SslConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("SSL certificate file not found: {0}")]
    CertMissing(PathBuf),
    #[error("SSL private key file not found: {0}")]
    KeyMissing(PathBuf),
    #[error("SSL certificate/key validation failed: {0}")]
    Invalid(String),
    #[error("Failed to parse certificate file: {0}")]
    Parse(String),
    #[error("SSL certificate has expired: {0}")]
    Expired(String),
    #[error("SSL certificate is not valid yet: {0}")]
    NotYetValid(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// `$XDG_DATA_HOME|~/.local/share` (Windows `%LOCALAPPDATA%`) `/mokuro-bunko/certs/{cert,key}.pem`.
pub fn default_cert_paths() -> (PathBuf, PathBuf) {
    let dir = bunko_core::storage::default_storage_path().join("certs");
    (dir.join("cert.pem"), dir.join("key.pem"))
}

/// Generate a self-signed certificate for `localhost`, `127.0.0.1` and this host name.
pub fn generate_self_signed(cert_path: &Path, key_path: &Path, hostname: &str) -> Result<(), TlsError> {
    let mut names = vec!["localhost".to_string(), hostname.to_string(), "127.0.0.1".to_string()];
    if let Ok(h) = hostname::get() {
        names.push(h.to_string_lossy().into_owned());
    }
    names.dedup();
    let mut params = rcgen::CertificateParams::new(names).map_err(|e| TlsError::Invalid(e.to_string()))?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "localhost");
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "mokuro-bunko");
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(365);
    let key = rcgen::KeyPair::generate().map_err(|e| TlsError::Invalid(e.to_string()))?;
    let cert = params.self_signed(&key).map_err(|e| TlsError::Invalid(e.to_string()))?;
    for p in [cert_path, key_path] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(cert_path, cert.pem())?;
    write_private(key_path, key.serialize_pem().as_bytes())?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

fn load_pair(cert: &Path, key: &Path) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let certs: Vec<_> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| TlsError::Invalid(e.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|e| TlsError::Invalid(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::Parse(format!("no certificate in {}", cert.display())));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| TlsError::Invalid(e.to_string()))?;
    Ok((certs, key))
}

/// Errors (first one fatal at startup) and warnings (expiring within 30 days).
pub fn validate_pair(cert: &Path, key: &Path) -> (Vec<String>, Vec<String>) {
    let mut errors = vec![];
    let mut warnings = vec![];
    let (certs, key_der) = match load_pair(cert, key) {
        Ok(p) => p,
        Err(e) => return (vec![e.to_string()], warnings),
    };
    if let Err(e) = build_server_config(certs.clone(), key_der) {
        errors.push(e.to_string());
        return (errors, warnings);
    }
    match x509_parser::parse_x509_certificate(&certs[0]) {
        Ok((_, parsed)) => {
            let validity = parsed.validity();
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let not_after = validity.not_after.timestamp();
            let not_before = validity.not_before.timestamp();
            if not_after < now {
                errors.push(TlsError::Expired(validity.not_after.to_string()).to_string());
            } else if not_before > now {
                errors.push(TlsError::NotYetValid(validity.not_before.to_string()).to_string());
            } else if not_after - now < 30 * 86_400 {
                warnings.push(format!("SSL certificate expires soon: {}", validity.not_after));
            }
        }
        Err(e) => errors.push(TlsError::Parse(e.to_string()).to_string()),
    }
    (errors, warnings)
}

fn build_server_config(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<rustls::ServerConfig, TlsError> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Invalid(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TlsError::Invalid(e.to_string()))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

/// The rustls config for the server, generating the auto certificate when needed.
pub fn server_config(ssl: &SslConfig) -> Result<Option<Arc<rustls::ServerConfig>>, TlsError> {
    if !ssl.enabled {
        return Ok(None);
    }
    let (cert, key) = if ssl.auto_cert {
        let (cert, key) = default_cert_paths();
        if !cert.exists() || !key.exists() {
            generate_self_signed(&cert, &key, "localhost")?;
        }
        (cert, key)
    } else {
        let cert = bunko_core::storage::expand_user(Path::new(&ssl.cert_file));
        let key = bunko_core::storage::expand_user(Path::new(&ssl.key_file));
        if !cert.is_file() {
            return Err(TlsError::CertMissing(cert));
        }
        if !key.is_file() {
            return Err(TlsError::KeyMissing(key));
        }
        (cert, key)
    };
    let (certs, key) = load_pair(&cert, &key)?;
    Ok(Some(Arc::new(build_server_config(certs, key)?)))
}

pub fn describe(ssl: &SslConfig) -> String {
    if !ssl.enabled {
        "SSL disabled".into()
    } else if ssl.auto_cert {
        format!("SSL enabled (auto-cert: {})", default_cert_paths().0.display())
    } else {
        format!("SSL enabled (cert: {})", ssl.cert_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let (c, k) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        generate_self_signed(&c, &k, "localhost").unwrap();
        let (errors, warnings) = validate_pair(&c, &k);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty());
        let ssl = SslConfig { enabled: true, auto_cert: false, cert_file: c.to_string_lossy().into(), key_file: k.to_string_lossy().into() };
        assert!(server_config(&ssl).unwrap().is_some());
    }
}
