//! `ssl *` (0.5.2 `ssl_cli.py`). The auto certificate lives at the default cert paths
//! (`<data dir>/mokuro-bunko/certs/`, not under `storage.base_path`, as 0.5.2).

use super::Ctx;
use crate::cfgfile;
use crate::cli::SslCmd;
use crate::out::{CmdResult, Fail, exit_with};
use crate::prompt;
use bunko_server::tls;
use std::path::{Path, PathBuf};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

pub fn run(ctx: &Ctx, cmd: SslCmd) -> CmdResult {
    let path = &ctx.config_path;
    match cmd {
        SslCmd::Enable { auto_cert, cert, key } => {
            if !(auto_cert || cert.is_some() && key.is_some()) {
                return Err(exit_with("Error: Provide --auto-cert or both --cert and --key"));
            }
            if cert.is_some() != key.is_some() {
                return Err(exit_with("Error: Both --cert and --key are required"));
            }
            let mut config = cfgfile::load_for_write(path)?;
            config.ssl.enabled = true;
            if auto_cert {
                config.ssl.auto_cert = true;
                config.ssl.cert_file.clear();
                config.ssl.key_file.clear();
                let (cert_path, key_path) = tls::default_cert_paths();
                if !cert_path.exists() {
                    println!("Generating self-signed certificate...");
                    tls::generate_self_signed(&cert_path, &key_path, "localhost").map_err(Fail::msg)?;
                    println!("Certificate: {}", cert_path.display());
                    println!("Key: {}", key_path.display());
                }
            } else {
                config.ssl.auto_cert = false;
                config.ssl.cert_file = cert.unwrap_or_default();
                config.ssl.key_file = key.unwrap_or_default();
            }
            cfgfile::save(&config, path)?;
            println!("SSL enabled");
        }
        SslCmd::Disable => {
            let mut config = cfgfile::load_for_write(path)?;
            config.ssl.enabled = false;
            config.ssl.auto_cert = false;
            cfgfile::save(&config, path)?;
            println!("SSL disabled");
        }
        SslCmd::Status => status(&cfgfile::load_effective(path)?)?,
        SslCmd::Generate { hostname, days } => {
            let (cert_path, key_path) = tls::default_cert_paths();
            if cert_path.exists()
                && !prompt::confirm(&format!("Certificate already exists at {}. Overwrite?", cert_path.display()), Some(false))?
            {
                return Ok(());
            }
            println!("Generating self-signed certificate for '{hostname}'...");
            tls::generate_self_signed_days(&cert_path, &key_path, &hostname, i64::from(days)).map_err(Fail::msg)?;
            println!("Certificate: {}", cert_path.display());
            println!("Key: {}", key_path.display());
        }
    }
    Ok(())
}

fn status(config: &bunko_core::Config) -> CmdResult {
    if !config.ssl.enabled {
        println!("SSL: disabled");
        return Ok(());
    }
    println!("SSL: enabled");
    let cert_path: PathBuf = if config.ssl.auto_cert {
        println!("Mode: auto-cert");
        tls::default_cert_paths().0
    } else {
        println!("Mode: custom certificate");
        bunko_core::storage::expand_user(Path::new(&config.ssl.cert_file))
    };
    println!("Certificate: {}", cert_path.display());
    if !cert_path.exists() {
        if config.ssl.auto_cert {
            println!("Certificate file not found (will be generated on server start)");
        } else {
            // 0.5.2 printed "(will be generated on server start)" here too, which is
            // wrong for a custom certificate: the server refuses to start instead.
            println!("Certificate file not found");
        }
        return Ok(());
    }
    match describe_cert(&cert_path) {
        Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
        Err(e) => eprintln!("Could not read certificate: {e}"),
    }
    Ok(())
}

/// `Subject:`, `Not before:`, `Not after:` and `SANs:` lines for a PEM certificate.
pub fn describe_cert(path: &Path) -> Result<Vec<String>, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&data).map_err(|e| e.to_string())?;
    let (_, cert) = X509Certificate::from_der(&pem.contents).map_err(|e| e.to_string())?;
    let validity = cert.validity();
    let mut lines = vec![
        format!("Subject: {}", rfc4514(cert.subject())),
        format!("Not before: {}", py_utc(validity.not_before.timestamp())),
        format!("Not after: {}", py_utc(validity.not_after.timestamp())),
    ];
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        let names: Vec<&str> = san
            .value
            .general_names
            .iter()
            .filter_map(|n| if let GeneralName::DNSName(d) = n { Some(*d) } else { None })
            .collect();
        if !names.is_empty() {
            lines.push(format!("SANs: {}", names.join(", ")));
        }
    }
    Ok(lines)
}

/// Python `str(datetime)` of an aware UTC datetime: `2026-01-01 00:00:00+00:00`.
fn py_utc(ts: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}+00:00",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        Err(_) => ts.to_string(),
    }
}

/// `cryptography`'s `Name.rfc4514_string()`: RDNs in reverse order, `,`-joined.
fn rfc4514(name: &x509_parser::x509::X509Name<'_>) -> String {
    let rdns: Vec<String> = name
        .iter_rdn()
        .map(|rdn| {
            rdn.iter()
                .map(|atv| {
                    let oid = atv.attr_type().to_id_string();
                    let key = match oid.as_str() {
                        "2.5.4.3" => "CN",
                        "2.5.4.7" => "L",
                        "2.5.4.8" => "ST",
                        "2.5.4.10" => "O",
                        "2.5.4.11" => "OU",
                        "2.5.4.6" => "C",
                        "2.5.4.9" => "STREET",
                        "0.9.2342.19200300.100.1.25" => "DC",
                        "0.9.2342.19200300.100.1.1" => "UID",
                        other => other,
                    }
                    .to_string();
                    let value = atv.as_str().map(escape_rfc4514).unwrap_or_else(|_| "#".to_string());
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect();
    rdns.into_iter().rev().collect::<Vec<_>>().join(",")
}

fn escape_rfc4514(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for (i, c) in v.chars().enumerate() {
        let edge = (i == 0 && (c == '#' || c == ' ')) || (i + 1 == v.chars().count() && c == ' ');
        if matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';') || edge {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_generated_cert() {
        let dir = tempfile::tempdir().unwrap();
        let (c, k) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        tls::generate_self_signed(&c, &k, "myhost.local").unwrap();
        let lines = describe_cert(&c).unwrap();
        assert_eq!(lines[0], "Subject: O=mokuro-bunko,CN=localhost");
        assert!(lines[1].starts_with("Not before: ") && lines[1].ends_with("+00:00"), "{lines:?}");
        assert!(lines[3].starts_with("SANs: localhost, myhost.local"), "{lines:?}");
    }

    #[test]
    fn escapes() {
        assert_eq!(escape_rfc4514("a,b"), "a\\,b");
        assert_eq!(escape_rfc4514(" x "), "\\ x\\ ");
    }
}
