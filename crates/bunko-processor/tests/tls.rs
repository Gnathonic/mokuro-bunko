//! `library.tls_verify` against a library with a self-signed certificate.

use std::sync::Arc;
use std::time::Duration;

use bunko_processor::client::http_client;
use bunko_processor::config::TlsVerify;
use bunko_processor::tls::client_config;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A one-route HTTPS server with a self-signed certificate for `localhost`; its port
/// and the certificate's PEM.
async fn https_server() -> (u16, String) {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pem = cert.pem();
    let certs = vec![CertificateDer::from(cert.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, pem)
}

async fn get(verify: &TlsVerify, port: u16) -> Result<String, reqwest::Error> {
    let http = http_client(
        &client_config(verify).unwrap(),
        Duration::from_secs(2),
        false,
    )
    .unwrap();
    http.get(format!("https://localhost:{port}/"))
        .send()
        .await?
        .text()
        .await
}

#[tokio::test]
async fn self_signed_libraries_need_a_pinned_certificate_or_no_verification() {
    let (port, pem) = https_server().await;
    let refused = get(&TlsVerify::Yes, port).await.unwrap_err();
    let reason = bunko_processor::tls::tls_failure(&refused).expect("a TLS refusal");
    assert!(
        matches!(reason, rustls::Error::InvalidCertificate(_)),
        "{reason:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.pem");
    std::fs::write(&path, &pem).unwrap();
    assert_eq!(get(&TlsVerify::Cert(path), port).await.unwrap(), "ok");
    assert_eq!(get(&TlsVerify::No, port).await.unwrap(), "ok");

    // A different certificate is not this library's.
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .unwrap()
        .cert
        .pem();
    let wrong = dir.path().join("other.pem");
    std::fs::write(&wrong, other).unwrap();
    assert!(get(&TlsVerify::Cert(wrong), port).await.is_err());
}
