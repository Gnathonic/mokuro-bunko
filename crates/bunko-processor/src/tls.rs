//! The one rustls client configuration every connection to the library uses (JSON
//! requests, archive downloads, result uploads and the WebSocket), built from
//! `library.tls_verify`.
//!
//! * `true` — the webpki roots.
//! * `false` — no certificate check at all (a self-signed library on a trusted LAN);
//!   handshake signatures are still verified.
//! * a path — trust only the certificates in that PEM file, either as CAs or, for a
//!   library's own self-signed leaf certificate, by exact match (webpki cannot use an
//!   end-entity certificate as its own trust anchor; 0.5.2's `cafile=` could).

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::config::TlsVerify;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TlsError(pub String);

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The client configuration for `verify`.
pub fn client_config(verify: &TlsVerify) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError(format!("TLS setup failed: {e}")))?;
    let config = match verify {
        TlsVerify::Yes => {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        TlsVerify::No => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(
                provider.signature_verification_algorithms,
            )))
            .with_no_client_auth(),
        TlsVerify::Cert(path) => {
            let pem = std::fs::read(path).map_err(|e| {
                TlsError(format!(
                    "library.tls_verify: could not read {}: {e}",
                    path.display()
                ))
            })?;
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
                .collect::<Result<_, _>>()
                .map_err(|e| {
                    TlsError(format!(
                        "library.tls_verify: {} is not a PEM certificate: {e}",
                        path.display()
                    ))
                })?;
            if certs.is_empty() {
                return Err(TlsError(format!(
                    "library.tls_verify: no certificate in {}",
                    path.display()
                )));
            }
            let mut roots = RootCertStore::empty();
            let (added, _ignored) = roots.add_parsable_certificates(certs.iter().cloned());
            let inner = if added > 0 {
                Some(
                    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                        .build()
                        .map_err(|e| TlsError(format!("library.tls_verify: {e}")))?,
                )
            } else {
                None
            };
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned {
                    pinned: certs,
                    inner,
                    algorithms: provider.signature_verification_algorithms,
                }))
                .with_no_client_auth()
        }
    };
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AnyCertificate(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// A certificate in the configured file is trusted by exact match; anything else must
/// chain to one of them.
#[derive(Debug)]
struct Pinned {
    pinned: Vec<CertificateDer<'static>>,
    inner: Option<Arc<WebPkiServerVerifier>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self
            .pinned
            .iter()
            .any(|c| c.as_ref() == end_entity.as_ref())
        {
            return Ok(ServerCertVerified::assertion());
        }
        match &self.inner {
            Some(inner) => {
                inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            }
            None => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The rustls error somewhere in an error chain, if any.
pub fn tls_failure(error: &(dyn std::error::Error + 'static)) -> Option<rustls::Error> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<rustls::Error>() {
            return Some(found.clone());
        }
        // `io::Error::source` skips the error it wraps; look inside it explicitly.
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(inner) = io.get_ref()
        {
            if let Some(found) = inner.downcast_ref::<rustls::Error>() {
                return Some(found.clone());
            }
            if let Some(found) = tls_failure(inner) {
                return Some(found);
            }
        }
        current = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_builds() {
        client_config(&TlsVerify::Yes).unwrap();
        client_config(&TlsVerify::No).unwrap();
        let missing = client_config(&TlsVerify::Cert("/nonexistent/cert.pem".into())).unwrap_err();
        assert!(missing.0.contains("could not read"), "{missing}");
    }
}
