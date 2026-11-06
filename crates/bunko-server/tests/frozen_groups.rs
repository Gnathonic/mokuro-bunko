//! The freeze, end to end (design 0.8 D5, `bunko_core::fold_groups`): a folder that lists
//! two or more spellings of one name holds a case-variant group, and until an admin
//! merges it by hand nothing changes in it. A library on a store that can hold such
//! groups (a case-sensitive disk) gets groups of archives, of sidecars and of series
//! folders, with sidecars, covers, numbered orphans, a corrupt sidecar, a kept upgrade
//! layer, physical and per-account corrections and upload records for both members;
//! beside them a series spelled like a reader's per-user name. Every WebDAV write
//! method against every member, the files rule v1 gives their archives (and would give,
//! once written), the folders holding them, and every background pass (the OCR walk and
//! install, the upgrade, the orphan and kept-layer sweeps, the corrupt-sidecar sweep,
//! covers and `.nocover` markers, correction adoption, the compile) then leave every
//! file byte for byte and every database row as they were, and every write answers
//! 409. Reads still reach the winner.
//!
//! Where the disk keeps one spelling per name (Linux casefold, macOS, Windows) no group
//! can exist: the second spellings are never made, and the single-spelling outcome is
//! asserted instead (the other spelling reaches the one entry, nothing is frozen).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bunko_core::generations::{Generation, default_generation};
use bunko_core::{Config, Role};
use bunko_db::{AuditDetails, NewAuditEvent, UserStatus};
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::ocr::collect::{CollectRequest, Outcome, SHADOWED};
use bunko_server::ocr::types::{FileFacts, Job};
use http::Request;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const LEGACY: &str = r#"{"version": "0.2.1", "title": "S", "volume": "V", "pages": [{"img_path": "001.png", "blocks": []}, {"img_path": "002.png", "blocks": []}, {"img_path": "003.png", "blocks": []}]}"#;
const HAYAI: &str = r#"{"version":"0.2.5","title":"S","volume":"V","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.png","blocks":[]},{"img_path":"002.png","blocks":[]},{"img_path":"003.png","blocks":[]}]}"#;
/// The old file as 0.7.0-beta.3's upgrade kept it beside the new one.
const KEPT: &str = r#"{"version":"0.2.1","title":"T","volume":"Vol","pages":[{"img_path":"001.png","blocks":[]},{"img_path":"002.png","blocks":[]},{"img_path":"003.png","blocks":[]}],"ocr_engine":{"id":"mokuro","generation":"mokuro","generator":"mokuro-bunko 0.7.0-beta.3","upgraded_from_primary":true}}"#;

fn sealed(content: &str) -> String {
    let mut v = bunko_layout::json::Value::parse(content).unwrap();
    bunko_layout::seal::seal(&mut v).expect("an ocr_engine block to seal")
}

/// A correction layer.
fn layer(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5",
        "pages": [{"img_path": "001.png", "blocks": [{"lines": [text]}]}],
    }))
    .unwrap()
}

/// A volume archive with `pages` PNG pages (a cover can be made from it), or none (a
/// `.nocover` marker would be written for it).
fn cbz(pages: usize, tag: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default();
        zw.start_file(format!("{tag}.txt"), options).unwrap();
        zw.write_all(tag.as_bytes()).unwrap();
        for i in 0..pages {
            zw.start_file(format!("{:03}.png", i + 1), options).unwrap();
            let mut png = Vec::new();
            image::RgbImage::from_pixel(40, 56, image::Rgb([200, 100, 50]))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            zw.write_all(&png).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library")).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.ocr.autobench = false;
        config.ocr.upgrade.enabled = true;
        config.ocr.upgrade.replace = vec!["mokuro-legacy".into(), "mokuro".into()];
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [
            ("admin", Role::Admin),
            ("alice", Role::Uploader),
            ("bob", Role::Uploader),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    fn library(&self) -> PathBuf {
        self.storage.join("library")
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.library().join(rel)
    }

    /// `rel` written with `bytes`, only when no entry of that name exists in any
    /// spelling the host joins. True when it was written.
    fn create_new(&self, rel: &str, bytes: &[u8]) -> bool {
        let path = self.lib(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(bytes).unwrap();
                true
            }
            Err(_) => false,
        }
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.lib(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    async fn send(
        &self,
        user: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, String) {
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
        );
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080")
            .header("authorization", auth)
            .header("content-length", body.len().to_string());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Every file below `root` with its bytes (and every folder), by relative path.
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if e.file_type().unwrap().is_dir() {
                out.insert(format!("{rel}/"), None);
                stack.push(path);
            } else {
                out.insert(rel, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

/// What the freeze guards: the library minus what Bunko may write outside the groups
/// (the ordinary `Free/` series, the root `catalog.json`, each `series.json` of a folder
/// that is no member), and every account's tree.
fn guarded(env: &Env) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out: BTreeMap<String, Option<Vec<u8>>> = tree(&env.library())
        .into_iter()
        .filter(|(rel, _)| {
            !rel.starts_with("Free/")
                && rel != "catalog.json"
                && !matches!(rel.as_str(), "S/series.json" | "T/series.json")
        })
        .map(|(rel, bytes)| (format!("library/{rel}"), bytes))
        .collect();
    out.extend(
        tree(&env.storage.join("users"))
            .into_iter()
            .map(|(rel, bytes)| (format!("users/{rel}"), bytes)),
    );
    out
}

/// Every row a freeze keeps: upload records, OCR provenance, series folder owners.
fn rows(env: &Env) -> Vec<String> {
    let db = &env.services.db;
    let mut out: Vec<String> = db
        .tracked_volume_uploaders()
        .unwrap()
        .into_iter()
        .map(|(k, u)| format!("upload {k} {u}"))
        .collect();
    out.extend(
        db.list_ocr_sidecars()
            .unwrap()
            .into_iter()
            .map(|r| format!("ocr {r:?}")),
    );
    out.extend(
        db.list_series_folder_owners()
            .unwrap()
            .into_iter()
            .map(|r| format!("owner {r:?}")),
    );
    for key in [
        "S/V.cbz",
        "S/v.cbz",
        "T/Vol.cbz",
        "Kingdom/a.cbz",
        "kingdom/a.cbz",
    ] {
        out.push(format!(
            "uuid {key} {:?}",
            db.remembered_volume_uuid(key).unwrap()
        ));
    }
    out
}

/// The library: groups of archives (`S/V.cbz`, `S/v.cbz`), of sidecars (`S/V.mokuro`,
/// `S/v.mokuro`; `T/VOL.mokuro`, `T/vol.mokuro`) and of series folders (`Kingdom/`,
/// `kingdom/`), the files around them, an ordinary series `Free/`, and a series
/// spelled like a per-user name (`History/`). True when the disk lists every second
/// spelling (a case-sensitive disk).
fn build(env: &Env) -> bool {
    env.write("Free/F.cbz", cbz(3, "free"));
    // S: two spellings of one archive, each with its own files.
    env.write("S/V.cbz", cbz(3, "winner"));
    let mut two = env.create_new("S/v.cbz", &cbz(0, "hidden"));
    env.write("S/V.mokuro", LEGACY);
    env.write("S/V.fast.mokuro", b"not json at all");
    env.write("S/V_1.mokuro", sealed(HAYAI));
    env.write("S/V.edit.mokuro", layer("physical correction"));
    if two {
        env.write("S/v.mokuro", HAYAI);
        env.write("S/v.webp", b"hidden cover");
        env.write("S/v.hayai.mokuro", sealed(HAYAI));
    }
    // T: two spellings of one archive's primary sidecar (rule v1 gives both to
    // `Vol.cbz`), and the layer an earlier beta's upgrade kept.
    env.write("T/Vol.cbz", cbz(3, "t"));
    env.write("T/VOL.mokuro", HAYAI);
    two &= env.create_new("T/vol.mokuro", LEGACY.as_bytes());
    env.write("T/VOL.mokuro.mokuro", KEPT);
    // Kingdom: two spellings of a series folder.
    env.write("Kingdom/a.cbz", cbz(3, "kingdom winner"));
    env.write("Kingdom/a.mokuro", b"{ corrupt");
    two &= env.create_new("kingdom/a.cbz", &cbz(2, "kingdom hidden"));
    // A series spelled like a reader's per-user folder.
    env.write("History/h.cbz", cbz(3, "history"));
    env.write("History/h.mokuro", b"{ corrupt");
    env.write("History/h.edit.mokuro", layer("history correction"));
    // Every account's own corrections of both spellings (and admin's of `Free/F.cbz`).
    let free = env.storage.join("users/admin/edits/Free");
    std::fs::create_dir_all(&free).unwrap();
    std::fs::write(free.join("F.edit.mokuro"), layer("admin on F")).unwrap();
    let edits = env.storage.join("users/alice/edits/S");
    std::fs::create_dir_all(&edits).unwrap();
    std::fs::write(edits.join("V.edit.mokuro"), layer("alice on V")).unwrap();
    if two {
        std::fs::write(edits.join("v.edit.mokuro"), layer("alice on v")).unwrap();
    }
    // Records: both members' uploaders, the last writers of the physical corrections
    // (adoption would hand them over), the kept layer's upgrade, OCR provenance.
    let db = &env.services.db;
    for (key, user) in [
        ("S/V.cbz", "bob"),
        ("S/v.cbz", "alice"),
        ("T/Vol.cbz", "bob"),
        ("Kingdom/a.cbz", "bob"),
        ("kingdom/a.cbz", "alice"),
        ("Free/F.cbz", "bob"),
    ] {
        db.record_volume_upload(key, user).unwrap();
    }
    for target in [
        "/mokuro-reader/S/V.edit.mokuro",
        "/mokuro-reader/History/h.edit.mokuro",
    ] {
        db.log_audit_event(
            &NewAuditEvent::new("upload")
                .actor(Some("alice"))
                .target_type("library")
                .target_path(target),
        )
        .unwrap();
    }
    db.log_audit_event(
        &NewAuditEvent::new("ocr_sidecar_upgraded")
            .target_type("sidecar")
            .target_path("/mokuro-reader/T/VOL.mokuro")
            .details(
                AuditDetails::new()
                    .with("kept_as", "T/VOL.mokuro.mokuro")
                    .with("mode", "generated"),
            ),
    )
    .unwrap();
    for (sidecar, volume) in [("S/V_1.mokuro", "S/V.cbz"), ("T/VOL.mokuro", "T/Vol.cbz")] {
        db.record_ocr_sidecar(&bunko_db::OcrSidecar {
            sidecar_path: sidecar.into(),
            volume_key: volume.into(),
            generation_id: "g-1".into(),
            generation_name: "hayai-nova".into(),
            ..Default::default()
        })
        .unwrap();
    }
    db.remember_volume_uuid("S/V.cbz", "uuid-v").unwrap();
    two
}

fn fast() -> Generation {
    let mut g = default_generation("g-2");
    g.name = "fast".into();
    g.primary = false;
    g
}

/// An OCR result for `rel` handed to the collector, as a processor's arrives.
fn collect(env: &Env, rel: &str, row: Generation, upgrade: bool) -> Outcome {
    let dir = env
        .storage
        .join(".processing")
        .join(format!("s-{}", rel.len()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("result.mokuro");
    std::fs::write(&out, HAYAI).unwrap();
    let layout = env.services.core.layout.clone();
    let job = if upgrade {
        Job::upgrade(rel, &row.id)
    } else {
        Job::new(rel, &row.id)
    };
    let req = CollectRequest {
        job,
        rows: vec![default_generation("g-1"), fast()],
        row,
        library: layout.library(),
        inbox: layout.inbox(),
        result: out,
        expected_sha256: None,
        stamp: None,
        machine: "local".into(),
        account: None,
        runner_build: None,
        pages: Some(3),
        failed_pages: Some(0),
        generator: "mokuro-bunko test".into(),
        db: Some(env.services.db.clone()),
        facts: Arc::new(FileFacts),
        locks: Arc::new(bunko_library::service::NoPathLocks),
        sid: "s-1".into(),
        claim: "c-1".into(),
        congestion: None,
        upgrade: Some(env.services.ocr.upgrade().clone()),
    };
    bunko_server::ocr::collect::run(req).outcome
}

/// Every background pass over the library, as the server runs them.
async fn background_passes(env: &Env) {
    let library = env.library();
    let up = env.services.ocr.upgrade().clone();
    let rows = vec![default_generation("g-1"), fast()];
    // The OCR walk: no volume of a group, nor of `History/`, is owed anything.
    let index = bunko_server::ocr::owed::walk(
        &library,
        &rows,
        &FileFacts,
        Some(up.as_ref() as &dyn bunko_server::ocr::owed::UpgradeProbe),
        &bunko_server::ocr::owed::KnownPages::new(),
    );
    let owed: Vec<&String> = index.volumes.keys().collect();
    assert_eq!(owed, ["Free/F.cbz"], "{owed:?}");
    assert!(index.orphans.is_empty(), "{:?}", index.orphans);
    for rel in [
        "S/V.cbz",
        "S/v.cbz",
        "T/Vol.cbz",
        "Kingdom/a.cbz",
        "kingdom/a.cbz",
    ] {
        for row in &rows {
            assert!(
                !bunko_server::ocr::owed::still_owed(&library, rel, row, false),
                "{rel} {}",
                row.name
            );
        }
    }
    // Results arriving for them (claimed before the group appeared): discarded.
    for (rel, row, upgrade) in [
        ("S/V.cbz", default_generation("g-1"), false),
        ("S/V.cbz", fast(), false),
        ("T/Vol.cbz", default_generation("g-1"), false),
        ("T/Vol.cbz", fast(), false),
        ("Kingdom/a.cbz", fast(), false),
        ("History/h.cbz", fast(), false),
        ("S/V.cbz", default_generation("g-1"), true),
    ] {
        let outcome = collect(env, rel, row, upgrade);
        assert!(
            !matches!(outcome, Outcome::Installed(_)),
            "{rel}: {outcome:?}"
        );
        if !upgrade {
            assert_eq!(outcome, Outcome::Superseded(SHADOWED.into()), "{rel}");
        }
    }
    // The upgrade: its census, its paced swaps, a direct swap, the kept-layer sweep.
    let up2 = up.clone();
    let lib2 = library.clone();
    tokio::task::spawn_blocking(move || {
        for rel in ["S/V.cbz", "T/Vol.cbz", "Kingdom/a.cbz"] {
            let _ = up2.judge(&lib2.join(rel), rel);
        }
        up2.run_direct(
            vec!["S/V.cbz".into(), "T/Vol.cbz".into(), "Kingdom/a.cbz".into()],
            Duration::ZERO,
        );
        let staged = lib2.parent().unwrap().join("staged.mokuro");
        std::fs::write(&staged, HAYAI).unwrap();
        assert!(
            up2.swap(&lib2.join("S/V.cbz"), &staged, "generated")
                .is_err()
        );
        assert!(
            up2.direct_replace(&lib2.join("T/Vol.cbz"), &lib2.join("T/VOL.mokuro.mokuro"))
                .is_err()
        );
        assert_eq!(up2.sweep_kept_from_audit(), 0);
    })
    .await
    .unwrap();
    // The orphan sweep: `S/V_1.mokuro` would take `V.mokuro`, a frozen archive's.
    let locks: bunko_server::ocr::types::PathLocks = Arc::new(bunko_library::service::NoPathLocks);
    let report = bunko_server::ocr::orphans::sweep(
        &library,
        Some(&env.services.db),
        &locks,
        &["S/V_1.mokuro".to_string()],
        Duration::ZERO,
    );
    assert_eq!((report.moved, report.removed, report.left), (0, 0, 1));
    // The corrupt-sidecar sweep: `S/V.fast.mokuro`, `Kingdom/a.mokuro` and
    // `History/h.mokuro` are corrupt, and frozen or no library entry.
    assert_eq!(
        bunko_server::thumbs::remove_corrupt_sidecars(&library, Some(&env.services.db)),
        0
    );
    // Covers and `.nocover` markers.
    let candidates = bunko_server::thumbs::thumbnail_candidates(&library);
    assert_eq!(candidates, [library.join("Free/F.cbz")], "{candidates:?}");
    let thumbs = bunko_server::thumbs::Thumbnails::default();
    assert_eq!(thumbs.scan_once(&library, &CancellationToken::new()), 1);
    assert_eq!(bunko_server::thumbs::clear_jxl_nocovers(&library), 0);
    // Adoption of the physical corrections: none leaves a group or `History/`.
    let sweep = bunko_server::library::adopt_unowned_corrections(
        &env.services.db,
        &env.services.dav.overlays(),
        true,
    );
    assert_eq!(sweep.adopted, 0, "{sweep:?}");
    // The compile (series files, the catalog, retiring stale ones).
    env.services.library.recompile_all_now().await;
}

/// The DAV write matrix. Every answer must be 409.
async fn every_write(env: &Env) -> Vec<String> {
    let mut wrong = Vec::new();
    let mut check = |what: String, (code, body): (u16, String)| {
        if code != 409 {
            wrong.push(format!("{what}: {code} {body}"));
        }
    };
    let proppatch = br#"<?xml version="1.0" encoding="utf-8"?><D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:x"><D:set><D:prop><Z:p>1</Z:p></D:prop></D:set></D:propertyupdate>"#;
    let lockinfo = br#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>t</D:owner></D:lockinfo>"#;
    let files = [
        // Members, the winner included.
        "S/V.cbz",
        "S/v.cbz",
        "S/V.CBZ",
        "T/VOL.mokuro",
        "T/vol.mokuro",
        "Kingdom/a.cbz",
        "kingdom/a.cbz",
        // Files rule v1 gives the frozen archives, listed or not yet.
        "S/V.mokuro",
        "S/v.mokuro",
        "S/v.webp",
        "S/V.webp",
        "S/v.mokuro.gz",
        "S/V.hayai.mokuro",
        "S/v.hayai.mokuro",
        "S/V.fast.mokuro",
        "S/v.nocover",
        "S/V.edit.mokuro",
        "S/v.edit.mokuro",
        "T/Vol.cbz",
        "T/Vol.webp",
        "T/VOL.mokuro.mokuro",
        "T/Vol.fast.mokuro",
        "Kingdom/a.mokuro",
        "Kingdom/new.cbz",
        "kingdom/new.mokuro",
    ];
    for (i, rel) in files.iter().enumerate() {
        let path = format!("/mokuro-reader/{rel}");
        check(
            format!("PUT {rel}"),
            env.send("admin", "PUT", &path, &[], b"x").await,
        );
        check(
            format!("DELETE {rel}"),
            env.send("admin", "DELETE", &path, &[], b"").await,
        );
        check(
            format!("MKCOL {rel}"),
            env.send("admin", "MKCOL", &path, &[], b"").await,
        );
        check(
            format!("PROPPATCH {rel}"),
            env.send("admin", "PROPPATCH", &path, &[], proppatch).await,
        );
        check(
            format!("LOCK {rel}"),
            env.send(
                "admin",
                "LOCK",
                &path,
                &[("timeout", "Second-60")],
                lockinfo,
            )
            .await,
        );
        let ext = rel.rsplit_once('.').map_or("x", |(_, e)| e);
        // A correction moves between correction names only (else 403 before anything
        // else): admin's own correction of `Free/F.cbz` stands in.
        let correction = rel.contains(".edit");
        let source = if correction {
            "/mokuro-reader/Free/F.edit.mokuro"
        } else {
            "/mokuro-reader/Free/F.cbz"
        };
        for method in ["COPY", "MOVE"] {
            let out = if correction {
                format!(
                    "http://localhost:8080/mokuro-reader/Free/F.edit-{}.mokuro",
                    i + 2
                )
            } else {
                format!("http://localhost:8080/mokuro-reader/Free/out-{i}.{ext}")
            };
            check(
                format!("{method} {rel} as source"),
                env.send("admin", method, &path, &[("destination", &out)], b"")
                    .await,
            );
            let to = format!("http://localhost:8080{path}");
            check(
                format!("{method} onto {rel}"),
                env.send(
                    "admin",
                    method,
                    source,
                    &[("destination", &to), ("overwrite", "T")],
                    b"",
                )
                .await,
            );
        }
    }
    // An account's own corrections of either spelling (its overlay).
    for rel in ["S/V.edit.mokuro", "S/v.edit.mokuro", "S/V.edit-2.mokuro"] {
        let path = format!("/mokuro-reader/{rel}");
        check(
            format!("alice PUT {rel}"),
            env.send("alice", "PUT", &path, &[], &layer("alice")).await,
        );
        check(
            format!("alice DELETE {rel}"),
            env.send("alice", "DELETE", &path, &[], b"").await,
        );
    }
    // Member folders, a new folder inside one, and the folders holding groups.
    for rel in ["Kingdom/", "kingdom/", "KINGDOM/", "Kingdom/sub/"] {
        let path = format!("/mokuro-reader/{rel}");
        for method in ["MKCOL", "DELETE", "PROPPATCH", "LOCK"] {
            let body: &[u8] = match method {
                "PROPPATCH" => proppatch,
                "LOCK" => lockinfo,
                _ => b"",
            };
            check(
                format!("{method} {rel}"),
                env.send("admin", method, &path, &[], body).await,
            );
        }
    }
    for rel in ["Kingdom/", "kingdom/", "S/", "T/"] {
        let path = format!("/mokuro-reader/{rel}");
        check(
            format!("DELETE {rel}"),
            env.send("admin", "DELETE", &path, &[], b"").await,
        );
        for method in ["COPY", "MOVE"] {
            let out = format!(
                "http://localhost:8080/mokuro-reader/Out-{}-{method}/",
                rel.trim_end_matches('/')
            );
            check(
                format!("{method} {rel} as source"),
                env.send(
                    "admin",
                    method,
                    &path,
                    &[("destination", &out), ("depth", "infinity")],
                    b"",
                )
                .await,
            );
            let to = format!("http://localhost:8080{path}");
            check(
                format!("{method} onto {rel}"),
                env.send(
                    "admin",
                    method,
                    "/mokuro-reader/Free/",
                    &[
                        ("destination", &to),
                        ("overwrite", "T"),
                        ("depth", "infinity"),
                    ],
                    b"",
                )
                .await,
            );
        }
    }
    wrong
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frozen_group_is_left_exactly_as_it_is() {
    let env = Env::new();
    let two = build(&env);
    let library = env.library();
    if !two {
        // One spelling per name on this disk: nothing is frozen, and another spelling
        // reaches the one entry.
        for rel in ["S/V.cbz", "T/VOL.mokuro", "Kingdom/a.cbz"] {
            assert_eq!(bunko_library::fsutil::frozen(&library, rel), None, "{rel}");
        }
        let (code, _) = env
            .send("admin", "GET", "/mokuro-reader/s/v.CBZ", &[], b"")
            .await;
        assert_eq!(code, 200);
        assert_eq!(std::fs::read(env.lib("S/V.cbz")).unwrap(), cbz(3, "winner"));
        assert!(
            bunko_server::storage::doctor::case_variant_rows(&library)
                .iter()
                .all(|r| r.label != "Case variants")
        );
        // The series spelled like a per-user folder is reserved on every host.
        assert!(matches!(
            bunko_library::fsutil::frozen(&library, "History/h.cbz"),
            Some(bunko_core::fold_groups::Frozen::Reserved { .. })
        ));
        let history = tree(&env.lib("History"));
        assert_eq!(
            bunko_server::thumbs::remove_corrupt_sidecars(&env.lib(""), None),
            1,
            "only Kingdom/a.mokuro, never History/h.mokuro"
        );
        let _ = bunko_server::library::adopt_unowned_corrections(
            &env.services.db,
            &env.services.dav.overlays(),
            true,
        );
        assert_eq!(tree(&env.lib("History")), history);
        return;
    }
    // Doctor names each group.
    let reported: Vec<String> = bunko_server::storage::doctor::case_variant_rows(&library)
        .into_iter()
        .filter(|r| r.label == "Case variants")
        .map(|r| r.detail)
        .collect();
    assert_eq!(reported.len(), 4, "{reported:?}");
    assert!(
        reported
            .iter()
            .any(|d| d.starts_with("S/V.cbz is used; hidden until merged: S/v.cbz")),
        "{reported:?}"
    );

    let files = guarded(&env);
    let records = rows(&env);

    let wrong = every_write(&env).await;
    assert!(
        wrong.is_empty(),
        "not refused with 409:\n{}",
        wrong.join("\n")
    );
    // Reads still reach the winner; the hidden spelling stays hidden.
    let (code, _) = env
        .send("admin", "GET", "/mokuro-reader/S/v.cbz", &[], b"")
        .await;
    assert_eq!(code, 200);
    let (code, listing) = env
        .send(
            "admin",
            "PROPFIND",
            "/mokuro-reader/S/",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert_eq!(code, 207);
    assert!(listing.contains("/mokuro-reader/S/V.cbz"), "{listing}");
    assert!(!listing.contains("/mokuro-reader/S/v.cbz"), "{listing}");

    background_passes(&env).await;

    let after = guarded(&env);
    let changed: Vec<&String> = files
        .keys()
        .chain(after.keys())
        .filter(|k| files.get(*k) != after.get(*k))
        .collect();
    assert!(changed.is_empty(), "changed: {changed:?}");
    assert_eq!(rows(&env), records);
}
