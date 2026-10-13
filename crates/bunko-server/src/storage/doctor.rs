//! `doctor`'s library storage rows (0.8 §7, §11, D25): where the library lives, the
//! Google pairing (and a pairing change waiting for its Disconnect), whether the spool
//! can hold what OCR spools at once, and how the credentials file is protected.
//! Offline: no provider is asked.

use std::path::Path;

use bunko_core::Config;
use bunko_core::config::LibraryBackend;

use super::connect::{pairing_label, provider_name};

/// One row's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Info,
    Warn,
    Fail,
}

/// One doctor row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub verdict: Verdict,
    pub label: &'static str,
    pub detail: String,
    pub hint: Option<String>,
}

fn row(
    verdict: Verdict,
    label: &'static str,
    detail: impl Into<String>,
    hint: Option<String>,
) -> Row {
    Row {
        verdict,
        label,
        detail: detail.into(),
        hint,
    }
}

/// The size assumed for the largest archive before any was spooled here.
pub const DEFAULT_LARGEST_ARCHIVE: u64 = 512 << 20;

/// The library storage rows for `config` (none but the first for a library on disk).
pub fn checks(config: &Config) -> Vec<Row> {
    let lib = &config.storage.library;
    if !config.is_cloud_library() {
        let mut out = vec![row(
            Verdict::Pass,
            "Library storage",
            format!(
                "on this disk ({})",
                config.storage.layout().library().display()
            ),
            None,
        )];
        out.extend(credentials_row(&config.storage.layout().cloud_dir()));
        return out;
    }
    let cloud = config.storage.layout().cloud_dir();
    let offline = super::describe_offline(config);
    let name = provider_name(lib.backend);
    let mut out = Vec::new();
    match offline["store_id"].as_str() {
        Some(id) => out.push(row(
            Verdict::Pass,
            "Library storage",
            format!(
                "{name}, folder {}/ ({}; store {id})",
                lib.folder,
                offline["account"].as_str().unwrap_or("signed in")
            ),
            None,
        )),
        None => out.push(row(
            Verdict::Fail,
            "Library storage",
            format!(
                "{name}, folder {}/: not connected ({})",
                lib.folder,
                offline["problem"].as_str().unwrap_or("no sign-in on file")
            ),
            Some(format!(
                "Connect it in the admin panel (Storage), or run: mokuro-bunko library connect {}",
                lib.backend.as_str()
            )),
        )),
    }
    if lib.backend == LibraryBackend::GDrive {
        let configured = super::pairing_of(lib);
        match super::pairing_change(lib, &cloud) {
            Some((configured, connected)) => out.push(row(
                Verdict::Fail,
                "Google pairing",
                format!(
                    "the pairing changed: configured {}, but the library on file is connected with {}; nothing is applied until the old library is disconnected",
                    pairing_label(&configured),
                    pairing_label(&connected)
                ),
                Some("Run `mokuro-bunko library disconnect` (or admin → Storage → Disconnect), then `mokuro-bunko library connect gdrive --reader <release|beta>`.".into()),
            )),
            None => out.push(row(
                Verdict::Pass,
                "Google pairing",
                pairing_label(&configured),
                None,
            )),
        }
    }
    out.push(spool_row(
        config,
        offline["store_id"].as_str().map(|id| cloud.join(id)),
    ));
    out.extend(credentials_row(&cloud));
    out
}

/// `storage.library.spool_gb` against 2 × lanes × the largest archive (the biggest one
/// spooled here so far, else [`DEFAULT_LARGEST_ARCHIVE`]).
fn spool_row(config: &Config, store_dir: Option<std::path::PathBuf>) -> Row {
    let lanes = u64::from(config.ocr.concurrency.max(1));
    let largest = store_dir
        .map(|d| largest_file(&d.join("spool")))
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LARGEST_ARCHIVE);
    let need = 2 * lanes * largest;
    let have = u64::from(config.storage.library.spool_gb) << 30;
    let gib = |n: u64| format!("{:.1} GiB", n as f64 / f64::from(1u32 << 30));
    let detail = format!(
        "spool_gb {} (needs at least {}: 2 × {lanes} lane{} × {} largest archive)",
        config.storage.library.spool_gb,
        gib(need),
        if lanes == 1 { "" } else { "s" },
        gib(largest)
    );
    if have >= need {
        row(Verdict::Pass, "Spool", detail, None)
    } else {
        row(
            Verdict::Warn,
            "Spool",
            detail,
            Some(format!(
                "Raise storage.library.spool_gb to {} or more: OCR waits for spool room otherwise.",
                need.div_ceil(1 << 30)
            )),
        )
    }
}

fn largest_file(dir: &Path) -> u64 {
    let Ok(rd) = bunko_store::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| bunko_store::fs::metadata(e.path()).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .max()
        .unwrap_or(0)
}

/// How `<cloud>/credentials.json` is protected: sealed with `MOKURO_SECRETS_KEY`, or
/// mode 0600 in a 0700 directory. Nothing when there is no file.
pub fn credentials_row(cloud: &Path) -> Option<Row> {
    let path = cloud.join(bunko_remote::auth::credentials::FILE_NAME);
    let text = bunko_store::fs::read_to_string(&path).ok()?;
    let sealed_file = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .is_some_and(|v| v.get("sealed").is_some());
    let key = bunko_remote::auth::SecretKey::from_env();
    match (&key, sealed_file) {
        (Err(e), _) => {
            return Some(row(
                Verdict::Fail,
                "Credentials",
                e.to_string(),
                Some(
                    "MOKURO_SECRETS_KEY must be 32 random bytes in base64: openssl rand -base64 32"
                        .into(),
                ),
            ));
        }
        (Ok(None), true) => {
            return Some(row(
                Verdict::Fail,
                "Credentials",
                format!("{} is sealed, and MOKURO_SECRETS_KEY is not set", path.display()),
                Some("Set MOKURO_SECRETS_KEY (or MOKURO_SECRETS_KEY_FILE) to the key it was sealed with.".into()),
            ));
        }
        (Ok(Some(k)), true) => {
            return Some(
                match bunko_remote::auth::CredentialStore::open_with_key(cloud, Some(k.clone())) {
                    Ok(_) => row(
                        Verdict::Pass,
                        "Credentials",
                        format!("{} is sealed (ChaCha20-Poly1305)", path.display()),
                        None,
                    ),
                    Err(e) => row(Verdict::Fail, "Credentials", e.to_string(), None),
                },
            );
        }
        _ => {}
    }
    let sealing = matches!(key, Ok(Some(_)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // unmounted: the credentials file's own mode bits.
        #[allow(clippy::disallowed_methods)]
        let modes = (
            std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777),
            std::fs::metadata(cloud).map(|m| m.permissions().mode() & 0o777),
        );
        if let (Ok(file), Ok(dir)) = modes {
            let private = file & 0o077 == 0 && dir & 0o077 == 0;
            let note = if sealing {
                " (MOKURO_SECRETS_KEY is set: the next write seals it)"
            } else {
                ""
            };
            return Some(if private {
                row(
                    Verdict::Pass,
                    "Credentials",
                    format!(
                        "{} is private (mode {file:o}, folder {dir:o}){note}",
                        path.display()
                    ),
                    None,
                )
            } else {
                row(
                    Verdict::Warn,
                    "Credentials",
                    format!(
                        "{} can be read by other users (mode {file:o}, folder {dir:o})",
                        path.display()
                    ),
                    Some(format!(
                        "chmod 600 {} && chmod 700 {} (or set MOKURO_SECRETS_KEY to seal it)",
                        path.display(),
                        cloud.display()
                    )),
                )
            });
        }
        None
    }
    #[cfg(not(unix))]
    {
        Some(row(
            if sealing {
                Verdict::Pass
            } else {
                Verdict::Info
            },
            "Credentials",
            format!(
                "{} is protected by your profile's permissions{}",
                path.display(),
                if sealing {
                    " (MOKURO_SECRETS_KEY is set: the next write seals it)"
                } else {
                    ""
                }
            ),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_config(dir: &Path, b: LibraryBackend) -> Config {
        let mut c = Config::default();
        c.storage.base_path = dir.to_path_buf();
        c.storage.library.backend = b;
        c
    }

    #[test]
    fn a_drive_library_without_a_sign_in_fails_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let c = cloud_config(dir.path(), LibraryBackend::GDrive);
        let rows = checks(&c);
        assert_eq!(rows[0].verdict, Verdict::Fail);
        assert!(
            rows[0]
                .hint
                .as_deref()
                .unwrap()
                .contains("library connect gdrive")
        );
        let pairing = rows.iter().find(|r| r.label == "Google pairing").unwrap();
        assert_eq!(pairing.verdict, Verdict::Pass);
        assert!(pairing.detail.contains("reader.mokuro.app"));
    }

    /// D25: a configured pairing other than the sign-in's is named, both of them.
    #[test]
    fn a_pairing_change_names_both_pairings() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cloud_config(dir.path(), LibraryBackend::GDrive);
        let cloud = c.storage.layout().cloud_dir();
        let creds = bunko_remote::auth::CredentialStore::open(&cloud).unwrap();
        let id = crate::storage::store_id("gdrive", "release", "acct", "mokuro-reader");
        creds
            .put(
                &id,
                bunko_remote::auth::StoredCredential {
                    provider: "gdrive".into(),
                    account_id: Some("acct".into()),
                    client_id: Some(bunko_remote::auth::clients::RELEASE_DEVICE_CLIENT_ID.into()),
                    refresh_token: Some("rt-secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            checks(&c).iter().all(|r| r.verdict != Verdict::Fail),
            "{:?}",
            checks(&c)
        );
        c.storage.library.google_project = bunko_core::config::GoogleProject::Beta;
        let rows = checks(&c);
        let p = rows.iter().find(|r| r.label == "Google pairing").unwrap();
        assert_eq!(p.verdict, Verdict::Fail);
        assert!(
            p.detail.contains("Mokuro Reader beta") && p.detail.contains("reader.mokuro.app"),
            "{}",
            p.detail
        );
        let all = format!("{rows:?}");
        assert!(!all.contains("rt-secret"), "{all}");
    }

    // A sparse fixture file is made with std::fs on purpose.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn spool_needs_two_archives_per_lane() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cloud_config(dir.path(), LibraryBackend::WebDav);
        c.storage.library.spool_gb = 1;
        c.ocr.concurrency = 2;
        let r = spool_row(&c, None);
        assert_eq!(r.verdict, Verdict::Warn, "{r:?}");
        assert!(r.detail.contains("2.0 GiB"), "{}", r.detail);
        c.storage.library.spool_gb = 4;
        assert_eq!(spool_row(&c, None).verdict, Verdict::Pass);
        // The largest spooled archive raises the estimate.
        let store = dir.path().join("store");
        bunko_store::fs::create_dir_all(store.join("spool")).unwrap();
        let f = std::fs::File::create(store.join("spool").join("big")).unwrap();
        f.set_len(3 << 30).unwrap();
        let r = spool_row(&c, Some(store));
        assert_eq!(r.verdict, Verdict::Warn, "{r:?}");
        assert!(r.hint.unwrap().contains("12"));
    }

    #[cfg(unix)]
    #[test]
    fn credentials_rows_judge_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        assert!(credentials_row(&cloud).is_none());
        let creds = bunko_remote::auth::CredentialStore::open_with_key(&cloud, None).unwrap();
        creds
            .put(
                "x",
                bunko_remote::auth::StoredCredential {
                    provider: "webdav".into(),
                    webdav_password: Some("pw-secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if std::env::var_os("MOKURO_SECRETS_KEY").is_some() {
            return;
        }
        let r = credentials_row(&cloud).unwrap();
        assert_eq!(r.verdict, Verdict::Pass, "{r:?}");
        let path = cloud.join("credentials.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r = credentials_row(&cloud).unwrap();
        assert_eq!(r.verdict, Verdict::Warn, "{r:?}");
        assert!(!format!("{r:?}").contains("pw-secret"));
    }
}
