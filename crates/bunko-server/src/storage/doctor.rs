//! `doctor`'s library storage rows (0.8 §7, §11, D25): where the library lives, the
//! Google pairing (and a pairing change waiting for its Disconnect), whether the spool
//! can hold what OCR spools at once, how the credentials file is protected, a local
//! series named like the reader's reading history (0.8), names a local library holds in
//! two spellings (case variants, which only a case-sensitive disk can hold), and files
//! the filename fold ties to a volume that sidecar names rule v1 does not give it.
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
        out.extend(reserved_series_rows(
            &config.storage.layout().library(),
            None,
        ));
        out.extend(case_variant_rows(&config.storage.layout().library()));
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

/// Library folders spelled like a per-user name in any case (`history/`, `History/`,
/// `Goals.json/`) that hold archives: 0.8 serves `/mokuro-reader/history` (in every
/// spelling) as each account's reading history, so such a series is out of every
/// reader's reach until it is renamed. One row per folder, naming its spelling. A cloud
/// folder (`cloud_folder`) can have one too, uploaded under 0.7 or by a reader writing
/// to the cloud directly; there it is also the progress user's reading history, which
/// that account may delete from. Only the running server can look at a cloud library
/// (doctor is offline): it logs this once the cloud library is in sync.
pub fn reserved_series_rows(library: &Path, cloud_folder: Option<&str>) -> Vec<Row> {
    let Ok(entries) = bunko_store::fs::read_dir(library) else {
        return Vec::new();
    };
    let mut folders: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| bunko_core::reader_tree::is_per_user_root_name(n))
        .collect();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|name| {
            let dir = library.join(&name);
            let archives = bunko_store::fs::read_dir(&dir)
                .ok()?
                .flatten()
                .filter(|e| {
                    bunko_library::pyunicode::lower(&e.file_name().to_string_lossy())
                        .ends_with(".cbz")
                })
                .count();
            (archives > 0).then(|| {
                let volumes = format!("{archives} volume{}", if archives == 1 { "" } else { "s" });
                let reserved = format!(
                    "/mokuro-reader/{}",
                    bunko_core::reader_tree::canonical_per_user_rel(&name).unwrap_or_default()
                );
                match cloud_folder {
                    None => row(
                        Verdict::Warn,
                        "Reserved series name",
                        format!(
                            "the series folder {name}/ ({volumes}) is hidden: 0.8 reserves {reserved} in every spelling for each account's own files"
                        ),
                        Some(format!(
                            "Stop Bunko and rename the series folder {} (e.g. to \"{name} (series)\").",
                            dir.display()
                        )),
                    ),
                    Some(folder) => row(
                        Verdict::Warn,
                        "Reserved series name",
                        format!(
                            "the cloud series folder {name}/ ({volumes}) is hidden: 0.8 reserves {reserved} in every spelling for each account's own files, and in a cloud folder it is the progress user's"
                        ),
                        Some(format!(
                            "Rename {folder}/{name}/ in the cloud (e.g. to \"{name} (series)\") before the progress user's reader syncs its own files there."
                        )),
                    ),
                }
            })
        })
        .collect()
}

/// Names a local library holds in two or more spellings (only a case-sensitive disk can:
/// made outside Bunko, or by a release before 0.5.3), one Warn row per set (a
/// case-variant group, [`bunko_core::fold_groups`]). Bunko reaches one spelling of a
/// name, the byte-smallest; the others are hidden from DAV and the catalog. The whole
/// group is frozen until it is merged by hand: the members, the winner included, the
/// files rule v1 gives their archives (named in the row) and everything inside a member
/// folder are never written, moved, deleted or re-keyed, by a client (409) or by Bunko
/// (no OCR, cover, sweep or adoption). Also the hidden names a case-only rename leaves behind when
/// it is interrupted between its two steps (`.bunko-case-<token>-<name>`), with the
/// rename that finishes it. Also, one Warn row each, a file the filename fold ties to a
/// listed volume's files that sidecar names rule v1 does not give it (`STRASSE.mokuro`
/// beside `Straße.cbz`, `V.EDIT.mokuro` beside `V.cbz`: Bunko writes nothing for the
/// volume in its place) or that only the fold gives a volume file suffix
/// ([`bunko_core::sidecar_names::fold_disputed`]), and one the fold makes one name with
/// a personal or curated layer name while it is no such layer (`X.ed\u{200B}it.mokuro`,
/// its volume listed or not: [`bunko_core::sidecar_names::shadows_reserved_layer`]), or
/// an archive whose OCR file could only be such a name (`X.ed\u{200B}it.cbz`). Also,
/// one Warn row per pair, two archives the fold makes one volume name while they spell
/// it otherwise (`Stra\u{df}e.cbz` beside `STRASSE.curated.cbz`,
/// [`bunko_core::sidecar_names::volume_name_tie_pairs`]). Walks
/// folders only (no file is read). A remote store merges its own case variants (spec §5.9): not for it.
pub fn case_variant_rows(library: &Path) -> Vec<Row> {
    let mut out = Vec::new();
    let mut todo = vec![String::new()];
    while let Some(rel) = todo.pop() {
        let dir = if rel.is_empty() {
            library.to_path_buf()
        } else {
            library.join(&rel)
        };
        let Ok(entries) = bunko_store::fs::read_dir(&dir) else {
            continue;
        };
        let mut listed: Vec<(String, bool)> = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Not followed: a symlinked folder is not walked (a link up the tree loops).
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            listed.push((name, is_dir));
        }
        listed.sort();
        let child = |name: &str| {
            if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            }
        };
        for (name, _) in &listed {
            if let Some(rest) = name.strip_prefix(bunko_store::fs::CASE_RENAME_PREFIX) {
                // `<token>-<name>`; a name too long to carry is `<token>` alone.
                let to = rest.split_once('-').map(|(_, to)| to);
                out.push(row(
                    Verdict::Warn,
                    "Interrupted case rename",
                    match to {
                        Some(to) => format!(
                            "{} is hidden: a case-only rename to {to} stopped half-way",
                            child(name)
                        ),
                        None => format!(
                            "{} is hidden: a case-only rename stopped half-way",
                            child(name)
                        ),
                    },
                    Some(match to {
                        Some(to) => format!(
                            "Stop Bunko and rename {} to {}.",
                            dir.join(name).display(),
                            dir.join(to).display()
                        ),
                        None => format!(
                            "Stop Bunko and rename {} back to the name it had.",
                            dir.join(name).display()
                        ),
                    }),
                ));
            }
        }
        let reachable: Vec<&(String, bool)> = listed
            .iter()
            .filter(|(n, _)| {
                !n.starts_with('.')
                    && !(rel.is_empty() && bunko_core::reader_tree::is_per_user_root_name(n))
            })
            .collect();
        let groups = bunko_library::fsutil::fold_groups(
            &dir,
            reachable.iter().map(|(n, is_dir)| (n.as_str(), *is_dir)),
        );
        let losers = groups.hidden();
        for group in groups.groups() {
            let shown = |n: &str| {
                let is_dir = listed.iter().any(|(m, d)| m == n && *d);
                if is_dir {
                    let volumes = bunko_library::compiler::archive_names(&dir.join(n)).len();
                    format!(
                        "{}/ ({volumes} volume{})",
                        child(n),
                        if volumes == 1 { "" } else { "s" }
                    )
                } else {
                    child(n)
                }
            };
            let hidden: Vec<String> = group
                .members
                .iter()
                .filter(|n| **n != group.winner)
                .map(|n| shown(n))
                .collect();
            let files = if group.files.is_empty() {
                String::new()
            } else {
                format!(
                    " (with {})",
                    group
                        .files
                        .iter()
                        .map(|n| child(n))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            out.push(row(
                Verdict::Warn,
                "Case variants",
                format!(
                    "{} is used; hidden until merged: {}. Frozen until then{files}: no upload, rename, delete, OCR or cover touches any of them",
                    shown(&group.winner),
                    hidden.join(", ")
                ),
                Some(format!(
                    "Stop Bunko, move each hidden spelling's volumes (an archive with its sidecars, cover and layers) into {} (or rename them apart), then rename their database rows and corrections (MIGRATING-0.7.md, \"Library paths are case-insensitive\").",
                    dir.join(&group.winner).display()
                )),
            ));
        }
        // Names the filename fold ties to a volume's files without rule v1 doing so
        // (`STRASSE.mokuro` beside `Straße.cbz`, `V.EDIT.mokuro` beside `V.cbz`), and
        // names only the fold gives a volume file suffix. A frozen group's are its row's.
        let files: Vec<&str> = reachable
            .iter()
            .filter(|(_, is_dir)| !is_dir)
            .map(|(n, _)| n.as_str())
            .collect();
        for name in &files {
            if groups.is_frozen(name) {
                continue;
            }
            let disputed = bunko_core::sidecar_names::fold_disputed(name, &files);
            let path = dir.join(name);
            if disputed.is_empty() {
                if bunko_core::sidecar_names::shadows_reserved_layer(name, &files).is_some() {
                    let what = if bunko_core::sidecar_names::shadowing_archive(name).is_some() {
                        "an archive whose OCR file would be an account's correction or the curated layer of the volume of the shorter stem, but its OCR file is no such layer by the sidecar naming rule Mokuro Reader shares"
                    } else {
                        "an account's correction or the curated layer of a volume of that stem, but no such layer by the sidecar naming rule Mokuro Reader shares: requests for those layers never reach it"
                    };
                    out.push(row(
                        Verdict::Warn,
                        "Ambiguous name",
                        format!(
                            "{} is one name on every host (Bunko joins names as case-insensitive filesystems do) with {what}, and Bunko writes no such name",
                            child(name)
                        ),
                        Some(format!("Delete {} or rename it apart.", path.display())),
                    ));
                }
                continue;
            }
            out.push(if disputed == [name.to_string()] {
                row(
                    Verdict::Warn,
                    "Ambiguous name",
                    format!(
                        "{} ends in a volume file suffix (.cbz, .mokuro, .mokuro.gz, .webp, .nocover) only as case-insensitive filesystems join names: it is no volume's file, and Bunko writes no such name",
                        child(name)
                    ),
                    Some(format!("Rename or delete {}.", path.display())),
                )
            } else {
                let volumes = disputed
                    .iter()
                    .map(|n| child(n))
                    .collect::<Vec<_>>()
                    .join(", ");
                row(
                    Verdict::Warn,
                    "Ambiguous name",
                    format!(
                        "{} is one name with a file of {volumes} on every host (Bunko joins names as case-insensitive filesystems do), but not that volume's file by the sidecar naming rule Mokuro Reader shares: Bunko writes nothing for the volume in its place, and only the volume's controller or an editor may change or delete it",
                        child(name)
                    ),
                    Some(format!(
                        "If it is that volume's file, rename {} to the volume's exact stem with a lowercase layer id; otherwise delete it or rename it apart.",
                        path.display()
                    )),
                )
            });
        }
        // Two archives the fold makes one volume name, spelled otherwise
        // (`Stra\u{df}e.cbz` beside `STRASSE.curated.cbz`): no request writes a file of
        // either volume until one is renamed or deleted. A frozen group's are its row's.
        for (a, b) in bunko_core::sidecar_names::volume_name_tie_pairs(&files) {
            if groups.is_frozen(a) || groups.is_frozen(b) {
                continue;
            }
            out.push(row(
                Verdict::Warn,
                "Ambiguous volume names",
                format!(
                    "{} and {} are one volume name on every host (Bunko joins names as case-insensitive filesystems do; a nested archive such as <stem>.curated.cbz is the volume <stem>'s), spelled otherwise: reads serve both, no request writes a file of either volume (409) until one of the archives is renamed or deleted, and Bunko writes none of its own files where the other volume's name is listed",
                    child(a),
                    child(b)
                ),
                Some(format!(
                    "Rename {} or {} (with its sidecars, cover and layers) so that both spell the volume name alike or the names differ, or delete the one that should not be there (MIGRATING-0.7.md, \"One volume name per fold\").",
                    dir.join(a).display(),
                    dir.join(b).display()
                )),
            ));
        }
        for (name, is_dir) in reachable {
            if *is_dir && !losers.contains(name) {
                todo.push(child(name));
            }
        }
    }
    out
}

/// `storage.library.spool_gb` against 2 × lanes × the largest archive (the biggest one
/// spooled here so far, at least [`DEFAULT_LARGEST_ARCHIVE`]).
fn spool_row(config: &Config, store_dir: Option<std::path::PathBuf>) -> Row {
    let lanes = u64::from(config.ocr.concurrency.max(1));
    let largest = store_dir
        .map(|d| largest_file(&d.join("spool")))
        .unwrap_or(0)
        .max(DEFAULT_LARGEST_ARCHIVE);
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

    /// 0.8 serves `/mokuro-reader/history` as each account's reading history, so a
    /// library series of that exact name can no longer be reached: named, with the fix.
    #[test]
    fn a_local_series_named_history_is_reported() {
        let reserved = |c: &Config| -> Vec<Row> {
            checks(c)
                .into_iter()
                .filter(|r| r.label == "Reserved series name")
                .collect()
        };
        // `History/` is the reserved name too: reported, by its own spelling.
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.storage.base_path = dir.path().to_path_buf();
        let library = c.storage.layout().library();
        bunko_store::fs::create_dir_all(library.join("History")).unwrap();
        bunko_store::fs::write(library.join("History/v.cbz"), "x").unwrap();
        let rows = reserved(&c);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].verdict, Verdict::Warn);
        assert!(
            rows[0].detail.contains("/mokuro-reader/history"),
            "{rows:?}"
        );
        assert!(rows[0].detail.contains("History/"), "{rows:?}");
        assert!(
            rows[0].hint.as_deref().unwrap().contains("rename"),
            "{rows:?}"
        );

        // In a fresh library: an empty `history/` passes, one holding an archive does not.
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.storage.base_path = dir.path().to_path_buf();
        let library = c.storage.layout().library();
        bunko_store::fs::create_dir_all(library.join("history")).unwrap();
        assert!(checks(&c).iter().all(|r| r.verdict == Verdict::Pass));
        bunko_store::fs::write(library.join("history/Vol 1.CBZ"), "x").unwrap();
        let rows = reserved(&c);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].verdict, Verdict::Warn);
        assert!(
            rows[0].detail.contains("/mokuro-reader/history"),
            "{rows:?}"
        );
        assert!(
            rows[0].hint.as_deref().unwrap().contains("rename"),
            "{rows:?}"
        );
    }

    /// Case variants (a case-sensitive disk only) are named with the spelling Bunko
    /// uses; an interrupted case-only rename is named with the rename that finishes it.
    /// The fixture writes the winner (byte-smallest) first, so a case-insensitive host
    /// holds one spelling and passes.
    #[test]
    fn case_variants_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.storage.base_path = dir.path().to_path_buf();
        let library = c.storage.layout().library();
        bunko_store::fs::create_dir_all(library.join("Kingdom")).unwrap();
        bunko_store::fs::write(library.join("Kingdom/a.cbz"), "x").unwrap();
        let _ = bunko_store::fs::create_dir(library.join("kingdom"));
        bunko_store::fs::write(library.join("kingdom/b.cbz"), "x").unwrap();
        let two = bunko_store::fs::read_dir(&library)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().eq_ignore_ascii_case("kingdom"))
            .count()
            == 2;
        let rows: Vec<Row> = checks(&c)
            .into_iter()
            .filter(|r| r.label == "Case variants")
            .collect();
        if two {
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].verdict, Verdict::Warn);
            assert!(
                rows[0].detail.starts_with("Kingdom/ (1 volume) is used"),
                "{rows:?}"
            );
            assert!(rows[0].detail.contains("kingdom/ (1 volume)"), "{rows:?}");
        } else {
            assert!(rows.is_empty(), "{rows:?}");
        }

        bunko_store::fs::create_dir_all(library.join("S")).unwrap();
        bunko_store::fs::write(library.join("S/.bunko-case-1a2b-V.cbz"), "x").unwrap();
        let rows: Vec<Row> = case_variant_rows(&library)
            .into_iter()
            .filter(|r| r.label == "Interrupted case rename")
            .collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].hint.as_deref().unwrap().contains("V.cbz"),
            "{rows:?}"
        );
    }

    /// A file the filename fold ties to a volume's files without rule v1 doing so (made
    /// before the volume, by hand, or by 0.7) is named, with the volume; nothing else is.
    #[test]
    fn ambiguous_names_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        bunko_store::fs::create_dir_all(library.join("S")).unwrap();
        for name in [
            "V.cbz",
            "V.mokuro",
            "V.webp",
            "V.EDIT.mokuro",
            "Stra\u{df}e.cbz",
            "STRASSE.mokuro",
            "W.mo\u{212A}uro",
            "X.ed\u{200B}it.mokuro",
            "Y.cura\u{200B}ted.cbz",
            "Notes.txt",
        ] {
            bunko_store::fs::write(library.join("S").join(name), "x").unwrap();
        }
        let mut rows: Vec<Row> = case_variant_rows(&library)
            .into_iter()
            .filter(|r| r.label == "Ambiguous name")
            .collect();
        rows.sort_by(|a, b| a.detail.cmp(&b.detail));
        let details: Vec<&str> = rows.iter().map(|r| r.detail.as_str()).collect();
        assert_eq!(rows.len(), 5, "{details:?}");
        // An archive whose OCR file could only stand in for `Y`'s curated layer.
        assert!(
            details.iter().any(|d| d.starts_with(
                "S/Y.cura\u{200B}ted.cbz is one name on every host (Bunko joins names as case-insensitive filesystems do) with an archive"
            )),
            "{details:?}"
        );
        // One name with an account's correction of a volume `X`, listed or not.
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("S/X.ed\u{200B}it.mokuro is one name on every host")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("S/V.EDIT.mokuro is one name with a file of S/V.cbz")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d
                    .starts_with("S/STRASSE.mokuro is one name with a file of S/Stra\u{df}e.cbz")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("S/W.mo\u{212A}uro ends in")),
            "{details:?}"
        );
        assert!(rows.iter().all(|r| r.verdict == Verdict::Warn));
    }

    /// Two archives the fold makes one volume name, spelled otherwise (made before this
    /// release, or by hand), are named together, once per pair; archives of one volume
    /// name as spelled are not.
    #[test]
    fn archives_of_one_volume_name_spelled_otherwise_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        bunko_store::fs::create_dir_all(library.join("S")).unwrap();
        for name in [
            "Stra\u{df}e.cbz",
            "STRASSE.curated.cbz",
            "X.cbz",
            "X\u{200B}.edit.cbz",
            "V.cbz",
            "V.curated.cbz",
            "W.cbz",
        ] {
            bunko_store::fs::write(library.join("S").join(name), "x").unwrap();
        }
        let mut details: Vec<String> = case_variant_rows(&library)
            .into_iter()
            .filter(|r| r.label == "Ambiguous volume names")
            .inspect(|r| assert_eq!(r.verdict, Verdict::Warn))
            .map(|r| r.detail)
            .collect();
        details.sort();
        assert_eq!(details.len(), 2, "{details:?}");
        assert!(
            details[0]
                .starts_with("S/STRASSE.curated.cbz and S/Stra\u{df}e.cbz are one volume name"),
            "{details:?}"
        );
        assert!(
            details[1].starts_with("S/X.cbz and S/X\u{200B}.edit.cbz are one volume name"),
            "{details:?}"
        );
    }

    /// A cloud folder can hold a `history/` series too (uploaded under 0.7, or by a
    /// reader writing to the cloud directly). Hidden the same way, and worse: it is the
    /// progress user's reading history now, which that account may delete from. Named
    /// with the folder to rename it in.
    #[test]
    fn a_cloud_series_named_history_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        bunko_store::fs::create_dir_all(library.join("history")).unwrap();
        bunko_store::fs::write(library.join("history/Vol 1.cbz"), "x").unwrap();
        let r = reserved_series_rows(&library, Some("mokuro-reader"))
            .pop()
            .expect("reported");
        assert_eq!(r.verdict, Verdict::Warn);
        assert!(r.detail.contains("progress user"), "{}", r.detail);
        let hint = r.hint.unwrap();
        assert!(hint.contains("mokuro-reader/history/"), "{hint}");
        assert!(!hint.contains(&*library.to_string_lossy()), "{hint}");
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
