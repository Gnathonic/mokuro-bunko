//! What counts as a person editing a volume's bare `.mokuro` (generation-upgrade.md §3,
//! "Not edited"), through the whole server: WebDAV writes land in the real audit log and
//! the upgrade census reads them back.
//!
//! Adding a volume together with its existing OCR file is not an edit; changing that file
//! afterwards is; re-sending its exact bytes is not.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::{AuditDetails, NewAuditEvent, OcrSidecar, UserStatus};
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::ocr::upgrade::{Upgrade, Verdict};
use http::{Request, StatusCode};
use tower::ServiceExt;

const LEGACY: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;
const HAYAI: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// The old file as 0.7.0-beta.3's upgrade kept it beside the new one.
const KEPT: &str = r#"{"version":"0.2.1","title":"A","volume":"V1","pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}],"ocr_engine":{"id":"mokuro","generation":"mokuro","generator":"mokuro-bunko 0.7.0-beta.3","upgraded_from_primary":true}}"#;
/// The same, with a reader's text correction.
const CORRECTED: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": [{"lines": ["fixed"]}]}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

fn cbz(pages: usize) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        for i in 0..pages {
            zw.start_file(
                format!("{:03}.jpg", i + 1),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(b"fake image bytes").unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
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
        services
            .db
            .create_user("admin", "pass1234", Role::Admin, UserStatus::Active, "")
            .unwrap();
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.storage.join("library").join(rel)
    }

    fn up(&self) -> &Arc<Upgrade> {
        self.services.ocr.upgrade()
    }

    async fn put(&self, path: &str, body: &[u8]) -> StatusCode {
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:pass1234")
        );
        let req = Request::builder()
            .method("PUT")
            .uri(path)
            .header("host", "localhost:8080")
            .header("authorization", auth)
            .header("content-length", body.len().to_string())
            .body(Body::from(body.to_vec()))
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }

    fn mkcol(&self, folder: &str) {
        std::fs::create_dir_all(self.lib(folder)).unwrap();
    }

    /// The census verdict, judged afresh (the census caches by the file's stat).
    fn judge(&self, rel: &str) -> Verdict {
        let up = self.up();
        let (config, rows) = (up.config.lock().clone(), up.rows.lock().clone());
        up.configure(&config, &rows);
        up.judge(&self.lib(rel), rel).1
    }

    /// A volume and its old OCR file, added the way Mokuro Reader adds them.
    async fn add_volume(&self, folder: &str) {
        self.mkcol(folder);
        let base = format!("/mokuro-reader/{folder}/V1");
        assert_eq!(self.put(&format!("{base}.cbz"), &cbz(3)).await, 201);
        assert_eq!(
            self.put(&format!("{base}.mokuro"), LEGACY.as_bytes()).await,
            201
        );
    }
}

#[tokio::test]
async fn a_volume_uploaded_with_its_ocr_file_is_not_edited() {
    let env = Env::new();
    env.add_volume("A").await;
    let rows = env.services.db.list_audit_events(100, None).unwrap();
    assert!(
        rows.iter().any(|e| e.action == "upload"
            && e.target_path.as_deref() == Some("/mokuro-reader/A/V1.mokuro")
            && e.actor_username.as_deref() == Some("admin")),
        "{rows:?}"
    );
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
}

#[tokio::test]
async fn re_sending_the_same_bytes_is_not_an_edit() {
    let env = Env::new();
    env.add_volume("A").await;
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", LEGACY.as_bytes())
            .await,
        204
    );
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
}

#[tokio::test]
async fn changing_the_ocr_file_is_an_edit_and_force_overrides_it() {
    let env = Env::new();
    env.add_volume("A").await;
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", CORRECTED.as_bytes())
            .await,
        204
    );
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
    let census = env.up().census();
    assert_eq!(census["skipped_edited"], 1);
    assert_eq!(census["edited_volumes"][0], "A/V1.cbz");
    // A later identical re-send does not hide the edit.
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", CORRECTED.as_bytes())
            .await,
        204
    );
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
    env.up().force("A/V1.cbz");
    assert_eq!(
        env.up().judge(&env.lib("A/V1.cbz"), "A/V1.cbz").1,
        Verdict::NeedsOcr
    );
}

#[tokio::test]
async fn a_revert_is_an_edit() {
    let env = Env::new();
    env.add_volume("A").await;
    env.services
        .db
        .log_audit_event(
            &NewAuditEvent::new("ocr_sidecar_reverted")
                .actor(Some("admin"))
                .target_type("library")
                .target_path("/mokuro-reader/A/V1.mokuro"),
        )
        .unwrap();
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
}

#[tokio::test]
async fn an_upload_row_from_an_older_database_is_a_first_upload() {
    let env = Env::new();
    env.mkcol("A");
    std::fs::write(env.lib("A/V1.cbz"), cbz(3)).unwrap();
    std::fs::write(env.lib("A/V1.mokuro"), LEGACY).unwrap();
    for details in [
        None,
        Some(AuditDetails::new().with("existed_before", false)),
    ] {
        let mut ev = NewAuditEvent::new("upload")
            .actor(Some("admin"))
            .target_type("library")
            .target_path("/mokuro-reader/A/V1.mokuro");
        ev.details = details;
        env.services.db.log_audit_event(&ev).unwrap();
    }
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
    // An `edit` row from before overwrites were compared still counts.
    env.services
        .db
        .log_audit_event(
            &NewAuditEvent::new("edit")
                .actor(Some("admin"))
                .target_type("library")
                .target_path("/mokuro-reader/A/V1.mokuro")
                .details(AuditDetails::new().with("existed_before", true)),
        )
        .unwrap();
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
}

#[tokio::test]
async fn a_file_newer_than_its_provenance_row_is_edited() {
    let env = Env::new();
    env.mkcol("A");
    std::fs::write(env.lib("A/V1.cbz"), cbz(3)).unwrap();
    std::fs::write(env.lib("A/V1.mokuro"), LEGACY).unwrap();
    env.services
        .db
        .record_ocr_sidecar(&OcrSidecar {
            sidecar_path: "A/V1.mokuro".into(),
            volume_key: "A/V1.cbz".into(),
            generation_id: "g-old".into(),
            generation_name: "mokuro".into(),
            machine: "local".into(),
            account: None,
            engine: Some("mokuro".into()),
            detector: Some("ctd".into()),
            precision: None,
            runner_build: None,
            pages: Some(3),
            failed_pages: Some(0),
            archive_size: None,
            archive_mtime_ns: None,
            written_at: String::new(),
        })
        .unwrap();
    assert_eq!(
        env.judge("A/V1.cbz"),
        Verdict::NeedsOcr,
        "row and file agree"
    );
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(env.lib("A/V1.mokuro"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
}

/// The `.mokuro` files of a series folder, sorted.
fn sidecars(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mokuro"))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn an_edited_volume_is_not_replaced_until_forced_and_then_keeps_no_layer() {
    let env = Env::new();
    env.add_volume("A").await;
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", CORRECTED.as_bytes())
            .await,
        204
    );
    std::fs::write(env.lib("A/V1.hayai-nova.mokuro"), HAYAI).unwrap();
    assert_eq!(env.judge("A/V1.cbz"), Verdict::SkippedEdited);
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        CORRECTED,
        "an edited file is left alone"
    );
    env.up().force("A/V1.cbz");
    let Verdict::Ready(layer) = env.up().judge(&env.lib("A/V1.cbz"), "A/V1.cbz").1 else {
        panic!("forced: the matching layer is ready to swap in");
    };
    env.up()
        .direct_replace(&env.lib("A/V1.cbz"), &layer)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        HAYAI
    );
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"]);
    let rows = env.services.db.list_audit_events(100, None).unwrap();
    let up = rows
        .iter()
        .find(|e| e.action == "ocr_sidecar_upgraded")
        .expect("audited");
    let details: serde_json::Value = serde_json::from_str(up.details.as_deref().unwrap()).unwrap();
    assert_eq!(details["replaced"], true);
    assert_eq!(details["mode"], "direct");
    assert!(details.get("kept_as").is_none(), "{details}");
    assert_eq!(env.judge("A/V1.cbz"), Verdict::Current);
}

fn beta3_upgrade_audit(env: &Env, bare: &str, kept: &str) {
    env.services
        .db
        .log_audit_event(
            &NewAuditEvent::new("ocr_sidecar_upgraded")
                .target_type("sidecar")
                .target_path(&format!("/mokuro-reader/{bare}"))
                .details(
                    AuditDetails::new()
                        .with("kept_as", kept)
                        .with("mode", "generated"),
                ),
        )
        .unwrap();
}

#[tokio::test]
async fn layers_beta3_kept_are_removed_at_startup_and_nothing_else() {
    use sha2::Digest;
    let env = Env::new();
    for folder in ["A", "B", "C"] {
        env.mkcol(folder);
        std::fs::write(env.lib(&format!("{folder}/V1.cbz")), cbz(3)).unwrap();
    }
    // A: upgraded; beta.3 kept the old file as a stamped layer, and its unstamped
    // original under `.upgrade-originals`.
    std::fs::write(env.lib("A/V1.mokuro"), HAYAI).unwrap();
    std::fs::write(env.lib("A/V1.mokuro.mokuro"), KEPT).unwrap();
    let digest = hex::encode(sha2::Sha256::digest(b"A/V1.mokuro.mokuro"));
    let backup = env
        .storage
        .join(".upgrade-originals")
        .join(format!("{}.mokuro", &digest[..32]));
    std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
    std::fs::write(&backup, LEGACY).unwrap();
    beta3_upgrade_audit(&env, "A/V1.mokuro", "A/V1.mokuro.mokuro");
    // B: a forced upgrade kept a person's edit unstamped; a person's own layer beside.
    std::fs::write(env.lib("B/V1.mokuro"), HAYAI).unwrap();
    std::fs::write(env.lib("B/V1.mokuro.mokuro"), CORRECTED).unwrap();
    std::fs::write(env.lib("B/V1.mine.mokuro"), CORRECTED).unwrap();
    beta3_upgrade_audit(&env, "B/V1.mokuro", "B/V1.mokuro.mokuro");
    // C: the bare file is not the upgraded output (the stamped copy a crash left).
    std::fs::write(env.lib("C/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(env.lib("C/V1.mokuro.mokuro"), KEPT).unwrap();
    beta3_upgrade_audit(&env, "C/V1.mokuro", "C/V1.mokuro.mokuro");

    assert_eq!(env.up().sweep_kept_from_audit(), 1);
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"]);
    assert!(!backup.exists());
    assert!(!env.storage.join(".upgrade-originals").exists());
    assert_eq!(
        sidecars(&env.lib("B")),
        ["V1.mine.mokuro", "V1.mokuro", "V1.mokuro.mokuro"]
    );
    assert_eq!(sidecars(&env.lib("C")), ["V1.mokuro", "V1.mokuro.mokuro"]);
    assert_eq!(env.up().sweep_kept_from_audit(), 0, "idempotent");
}
