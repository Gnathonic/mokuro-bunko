//! The main `<Vol>.mokuro` is model output (D4, generation-upgrade.md §3), through the
//! whole server on a local library: WebDAV writes land in the real audit log and the
//! upgrade census never skips a volume for them. An outdated main sidecar is replaced
//! outright, with no copy kept, whoever wrote it (stock mokuro, one a person PUT over,
//! one whose `ocr_engine` stamp anyone could have written, or Bunko's own sealed bytes:
//! owner decision, 2026-11-01). A layer becomes the main sidecar only when it is proven
//! Bunko's own by its seal and its own file carries the matching stamp, and unstamped
//! layers are never read or deleted. The same rules on a cloud library:
//! `cloud_upgrade.rs`.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::{AuditDetails, NewAuditEvent, OcrSidecar, UserStatus};
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::ocr::upgrade::{Upgrade, Verdict};
use http::{Request, StatusCode};
use serde_json::Value;
use sha2::Digest;
use tower::ServiceExt;

const LEGACY: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;
const HAYAI: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// HAYAI with a person's correction, its stamp left in.
const HAYAI_EDITED: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[{"lines":["fixed"]}]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// HAYAI as a reader pushes a layer back: the same pages, no stamp.
const HAYAI_UNSTAMPED: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// An old recipe Bunko itself wrote (0.5's served mokuro carries its stamp).
const BUNKO_OLD: &str = r#"{"version":"0.2.1","title":"A","volume":"V1","ocr_engine":{"id":"mokuro","generator":"mokuro-bunko 0.5.3"},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// BUNKO_OLD with a person's correction, its stamp left in.
const BUNKO_OLD_EDITED: &str = r#"{"version":"0.2.1","title":"A","volume":"V1","ocr_engine":{"id":"mokuro","generator":"mokuro-bunko 0.5.3"},"pages":[{"img_path":"001.jpg","blocks":[{"lines":["fixed"]}]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
/// Neither `ocr_engine` nor `version`: no recipe can be read.
const UNKNOWN: &str = r#"{"title":"A","volume":"V1","pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
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

fn sha(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// `content` as Bunko's OCR install leaves it: sealed (D4's proof, carried by the file).
fn sealed(content: &str) -> String {
    let mut v = bunko_layout::json::Value::parse(content).unwrap();
    bunko_layout::seal::seal(&mut v).expect("an ocr_engine block to seal")
}

fn is_sealed(path: &Path) -> bool {
    bunko_layout::seal::is_sealed(&std::fs::read(path).unwrap())
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
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
        put_via(&self.app, path, body).await
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

    /// A volume written straight to disk: its archive and its main sidecar.
    fn volume_on_disk(&self, folder: &str, main: &str) {
        self.mkcol(folder);
        std::fs::write(self.lib(&format!("{folder}/V1.cbz")), cbz(3)).unwrap();
        std::fs::write(self.lib(&format!("{folder}/V1.mokuro")), main).unwrap();
    }

    /// Judge, expect a matching layer, and swap it in.
    fn upgrade_direct(&self, rel: &str) -> Result<PathBuf, String> {
        let Verdict::Ready(layer) = self.judge(rel) else {
            panic!("{rel}: a matching layer is ready to swap in");
        };
        self.up().direct_replace(&self.lib(rel), &layer)
    }

    /// An upgrade job's result, swapped in the way the collector does.
    fn upgrade_generated(&self, rel: &str, result: &str) -> Result<PathBuf, String> {
        let out = self
            ._dir
            .path()
            .join(format!("result-{}.mokuro", sha(rel.as_bytes())));
        std::fs::write(&out, result).unwrap();
        self.up().swap(&self.lib(rel), &out, "generated")
    }

    /// The details of every `ocr_sidecar_upgraded` audit row, newest first.
    fn upgrade_audits(&self) -> Vec<Value> {
        self.services
            .db
            .list_audit_events(200, None)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "ocr_sidecar_upgraded")
            .map(|e| serde_json::from_str(e.details.as_deref().unwrap()).unwrap())
            .collect()
    }

    /// `content` at `rel` as Bunko's OCR install writes it: sealed (the proof D4 asks
    /// for; a stamp alone proves nothing), with its provenance row.
    fn bunko_wrote(&self, rel: &str, content: &str, engine: &str, detector: &str) {
        std::fs::write(self.lib(rel), sealed(content)).unwrap();
        self.row(rel, engine, detector);
    }

    /// A hayai-nova layer Bunko wrote.
    fn bunko_layer(&self, rel: &str) {
        self.bunko_wrote(rel, HAYAI, "hayai-nova", "ppocr-manga");
    }

    fn row(&self, rel: &str, engine: &str, detector: &str) {
        self.services
            .db
            .record_ocr_sidecar(&OcrSidecar {
                sidecar_path: rel.into(),
                volume_key: format!("{}.cbz", rel.split('.').next().unwrap()),
                generation_id: "g-x".into(),
                generation_name: engine.into(),
                machine: "local".into(),
                engine: Some(engine.into()),
                detector: Some(detector.into()),
                pages: Some(3),
                failed_pages: Some(0),
                ..Default::default()
            })
            .unwrap();
    }
}

async fn put_via(app: &Router, path: &str, body: &[u8]) -> StatusCode {
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
    app.clone().oneshot(req).await.unwrap().status()
}

/// The `.mokuro` files of a series folder, sorted.
fn sidecars(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mokuro"))
        .collect();
    out.sort();
    out
}

/// The upgrade kept no copy of the file it replaced: the audit names none.
fn assert_no_copy(env: &Env) {
    let audit = &env.upgrade_audits()[0];
    assert_eq!(audit["replaced"], true, "{audit}");
    assert!(audit.get("preserved_as").is_none(), "{audit}");
    assert!(audit.get("kept_as").is_none(), "{audit}");
}

/// L127/L141/L153: adding a volume with its OCR file, re-sending it and changing it
/// all leave the volume to the upgrade, which replaces the changed file outright.
#[tokio::test]
async fn a_stamp_less_main_sidecar_is_upgraded_with_no_copy() {
    let env = Env::new();
    env.add_volume("A").await;
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", LEGACY.as_bytes())
            .await,
        204
    );
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", CORRECTED.as_bytes())
            .await,
        204
    );
    assert_eq!(
        env.judge("A/V1.cbz"),
        Verdict::NeedsOcr,
        "an edit skips nothing"
    );
    let census = env.up().census();
    assert_eq!(census["needs_ocr"], 1);
    assert!(census.get("skipped_edited").is_none(), "{census}");
    assert!(census.get("edited_volumes").is_none(), "{census}");

    env.bunko_layer("A/V1.hayai-nova.mokuro");
    env.upgrade_direct("A/V1.cbz").unwrap();
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_eq!(
        sidecars(&env.lib("A")),
        ["V1.mokuro"],
        "no copy of the person's file; the layer copied in is not kept twice"
    );
    assert_eq!(env.upgrade_audits()[0]["mode"], "direct");
    assert_no_copy(&env);
    assert_eq!(env.judge("A/V1.cbz"), Verdict::Current);

    // Stock mokuro output, upgraded by a job: replaced the same way.
    env.volume_on_disk("B", LEGACY);
    assert_eq!(env.judge("B/V1.cbz"), Verdict::NeedsOcr);
    env.upgrade_generated("B/V1.cbz", HAYAI).unwrap();
    assert_eq!(
        json_of(&env.lib("B/V1.mokuro"))["ocr_engine"]["id"],
        "hayai-nova"
    );
    assert_eq!(sidecars(&env.lib("B")), ["V1.mokuro"]);
    assert_eq!(env.upgrade_audits()[0]["mode"], "generated");
    assert_no_copy(&env);
    assert_eq!(env.judge("B/V1.cbz"), Verdict::Current);
}

/// L180/L196/L228: the audit rows and mtimes that used to mark a file edited (a
/// revert, an `edit` row from an older database, a file newer than its row) skip
/// nothing any more.
#[tokio::test]
async fn reverts_edit_rows_and_newer_files_no_longer_skip_a_volume() {
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
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
    env.row("A/V1.mokuro", "mokuro", "ctd");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(env.lib("A/V1.mokuro"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(env.judge("A/V1.cbz"), Verdict::NeedsOcr);
}

/// A hand-made layer named `pre-upgrade` or `previous-ocr` (a stray `preserved` block
/// in it) is a layer like any other: never read, rewritten,
/// renumbered or deleted by an upgrade, which writes no layer of its own.
#[tokio::test]
async fn pre_upgrade_and_previous_ocr_layers_are_left_alone_by_an_upgrade() {
    let env = Env::new();
    env.volume_on_disk("A", LEGACY);
    let stray = r#"{"version":"0.2.1","pages":[],"preserved":{"by":"mokuro-bunko","sha256":"00"}}"#;
    std::fs::write(env.lib("A/V1.pre-upgrade.mokuro"), stray).unwrap();
    std::fs::write(env.lib("A/V1.previous-ocr.mokuro"), CORRECTED).unwrap();
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    env.upgrade_direct("A/V1.cbz").unwrap();
    assert_eq!(
        sidecars(&env.lib("A")),
        [
            "V1.mokuro",
            "V1.pre-upgrade.mokuro",
            "V1.previous-ocr.mokuro"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.pre-upgrade.mokuro")).unwrap(),
        stray
    );
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.previous-ocr.mokuro")).unwrap(),
        CORRECTED
    );
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_no_copy(&env);

    // Generated, the same: the job's result replaces the main and nothing else moves.
    env.volume_on_disk("B", CORRECTED);
    std::fs::write(env.lib("B/V1.pre-upgrade.mokuro"), stray).unwrap();
    env.upgrade_generated("B/V1.cbz", HAYAI).unwrap();
    assert_eq!(
        sidecars(&env.lib("B")),
        ["V1.mokuro", "V1.pre-upgrade.mokuro"]
    );
    assert_no_copy(&env);
    assert_eq!(env.judge("B/V1.cbz"), Verdict::Current);
}

/// A main sidecar Bunko wrote (sealed, with its row) is replaced with no copy, and its
/// replacement is sealed in turn.
#[tokio::test]
async fn a_main_sidecar_proven_bunkos_own_is_replaced_without_a_copy() {
    let env = Env::new();
    env.volume_on_disk("A", BUNKO_OLD);
    env.bunko_wrote("A/V1.mokuro", BUNKO_OLD, "mokuro", "ctd");
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    env.upgrade_direct("A/V1.cbz").unwrap();
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"]);
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_no_copy(&env);
    assert!(
        is_sealed(&env.lib("A/V1.mokuro")),
        "the new main sidecar is proven in turn"
    );

    env.volume_on_disk("B", BUNKO_OLD);
    env.bunko_wrote("B/V1.mokuro", BUNKO_OLD, "mokuro", "ctd");
    env.upgrade_generated("B/V1.cbz", HAYAI).unwrap();
    assert_eq!(sidecars(&env.lib("B")), ["V1.mokuro"]);
}

/// A library moved to another Bunko (a new instance, a new database, no provenance
/// rows) has its sealed main sidecars replaced with no copy, and an edit of one, its
/// seal broken, the same way.
#[tokio::test]
async fn a_moved_library_is_upgraded_without_any_database_row() {
    let env = Env::new();
    // A: Bunko's sealed bytes, copied in from another machine (no row here).
    env.volume_on_disk("A", BUNKO_OLD);
    std::fs::write(env.lib("A/V1.mokuro"), sealed(BUNKO_OLD)).unwrap();
    assert!(
        env.services
            .db
            .get_ocr_sidecar("A/V1.mokuro")
            .unwrap()
            .is_none()
    );
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    env.upgrade_direct("A/V1.cbz").unwrap();
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"], "no copy");
    assert_no_copy(&env);

    // B: the same file, its text corrected afterwards with the seal left in.
    env.volume_on_disk("B", BUNKO_OLD);
    let edited =
        sealed(BUNKO_OLD).replacen("\"blocks\":[]", "\"blocks\":[{\"lines\":[\"直した\"]}]", 1);
    assert_ne!(edited, sealed(BUNKO_OLD), "the fixture has a page to edit");
    std::fs::write(env.lib("B/V1.mokuro"), &edited).unwrap();
    env.bunko_layer("B/V1.hayai-nova.mokuro");
    env.upgrade_direct("B/V1.cbz").unwrap();
    assert_eq!(sidecars(&env.lib("B")), ["V1.mokuro"]);
    assert_eq!(
        std::fs::read_to_string(env.lib("B/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_no_copy(&env);
}

/// A reader's upload that carries Bunko's `ocr_engine` stamp, and Bunko's own file
/// edited afterwards with the stamp left in, are outdated main sidecars like any other:
/// replaced outright, with no copy.
#[tokio::test]
async fn a_forged_or_kept_stamp_on_an_edited_main_sidecar_is_upgraded_with_no_copy() {
    let env = Env::new();
    // A: a WebDAV PUT of a stamped file (no row: the PUT forgets it).
    env.mkcol("A");
    assert_eq!(env.put("/mokuro-reader/A/V1.cbz", &cbz(3)).await, 201);
    assert_eq!(
        env.put("/mokuro-reader/A/V1.mokuro", BUNKO_OLD.as_bytes())
            .await,
        201
    );
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    env.upgrade_direct("A/V1.cbz").unwrap();
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"]);
    assert_no_copy(&env);

    // B: Bunko's own file, then corrected on disk with the stamp left in: the row
    // still names Bunko's write, but not these bytes.
    env.volume_on_disk("B", BUNKO_OLD);
    env.bunko_wrote("B/V1.mokuro", BUNKO_OLD, "mokuro", "ctd");
    std::fs::write(env.lib("B/V1.mokuro"), BUNKO_OLD_EDITED).unwrap();
    env.bunko_layer("B/V1.hayai-nova.mokuro");
    env.upgrade_direct("B/V1.cbz").unwrap();
    assert_eq!(
        std::fs::read_to_string(env.lib("B/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_eq!(sidecars(&env.lib("B")), ["V1.mokuro"]);

    // C: a stamped file on disk with no row, upgraded by a job: the same.
    env.volume_on_disk("C", BUNKO_OLD);
    env.upgrade_generated("C/V1.cbz", HAYAI).unwrap();
    assert_eq!(sidecars(&env.lib("C")), ["V1.mokuro"]);
    assert_no_copy(&env);
}

#[tokio::test]
async fn a_layer_is_swapped_in_only_when_its_own_file_carries_the_stamp() {
    let env = Env::new();
    // A: the stamped layer is ready.
    env.volume_on_disk("A", LEGACY);
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    assert_eq!(
        env.judge("A/V1.cbz"),
        Verdict::Ready(env.lib("A/V1.hayai-nova.mokuro"))
    );
    // B: a reader pushed the layer back without the stamp; its row still says
    // hayai-nova. Never ready, never copied in, never deleted.
    env.volume_on_disk("B", LEGACY);
    std::fs::write(env.lib("B/V1.hayai-nova.mokuro"), HAYAI_UNSTAMPED).unwrap();
    env.row("B/V1.hayai-nova.mokuro", "hayai-nova", "ppocr-manga");
    // An unstamped layer with no row (a person's own) beside it.
    std::fs::write(env.lib("B/V1.mine.mokuro"), HAYAI_UNSTAMPED).unwrap();
    assert_eq!(env.judge("B/V1.cbz"), Verdict::NeedsOcr);
    let err = env
        .up()
        .direct_replace(&env.lib("B/V1.cbz"), &env.lib("B/V1.hayai-nova.mokuro"))
        .unwrap_err();
    assert!(err.contains("stamp"), "{err}");
    assert_eq!(
        std::fs::read_to_string(env.lib("B/V1.mokuro")).unwrap(),
        LEGACY
    );
    env.upgrade_generated("B/V1.cbz", HAYAI).unwrap();
    assert_eq!(
        sidecars(&env.lib("B")),
        ["V1.hayai-nova.mokuro", "V1.mine.mokuro", "V1.mokuro"]
    );
    for layer in ["B/V1.hayai-nova.mokuro", "B/V1.mine.mokuro"] {
        assert_eq!(
            std::fs::read_to_string(env.lib(layer)).unwrap(),
            HAYAI_UNSTAMPED,
            "{layer} untouched"
        );
    }
    assert!(
        env.services
            .db
            .get_ocr_sidecar("B/V1.hayai-nova.mokuro")
            .unwrap()
            .is_some(),
        "its row stays"
    );
    // Once upgraded, the census sweep leaves every unstamped layer alone too.
    assert_eq!(env.judge("B/V1.cbz"), Verdict::Current);
    assert_eq!(sidecars(&env.lib("B")).len(), 3);

    // C: a reader uploads a layer carrying the matching stamp (no proof Bunko wrote
    // it); D: Bunko's own layer, corrected on disk afterwards with its stamp left in.
    // Neither is a source: a forged stamp would make a person's file the model output.
    env.mkcol("C");
    assert_eq!(env.put("/mokuro-reader/C/V1.cbz", &cbz(3)).await, 201);
    assert_eq!(
        env.put("/mokuro-reader/C/V1.mokuro", LEGACY.as_bytes())
            .await,
        201
    );
    assert_eq!(
        env.put("/mokuro-reader/C/V1.hayai-nova.mokuro", HAYAI.as_bytes())
            .await,
        201
    );
    env.volume_on_disk("D", LEGACY);
    env.bunko_layer("D/V1.hayai-nova.mokuro");
    std::fs::write(env.lib("D/V1.hayai-nova.mokuro"), HAYAI_EDITED).unwrap();
    for v in ["C", "D"] {
        let (cbz, layer) = (
            env.lib(&format!("{v}/V1.cbz")),
            env.lib(&format!("{v}/V1.hayai-nova.mokuro")),
        );
        assert_eq!(env.judge(&format!("{v}/V1.cbz")), Verdict::NeedsOcr, "{v}");
        let err = env.up().direct_replace(&cbz, &layer).unwrap_err();
        assert!(err.contains("not proven"), "{v}: {err}");
        let err = env.up().swap(&cbz, &layer, "direct").unwrap_err();
        assert!(err.contains("not proven"), "{v}: {err}");
        assert_eq!(
            std::fs::read_to_string(env.lib(&format!("{v}/V1.mokuro"))).unwrap(),
            LEGACY,
            "{v}"
        );
        assert_eq!(sidecars(&env.lib(v)), ["V1.hayai-nova.mokuro", "V1.mokuro"]);
    }
}

#[tokio::test]
async fn an_unknown_main_sidecar_is_never_upgraded() {
    let env = Env::new();
    env.volume_on_disk("A", UNKNOWN);
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    assert_eq!(env.judge("A/V1.cbz"), Verdict::Current);
    assert_eq!(env.up().census()["families"]["unknown"], 1);
    let err = env.upgrade_generated("A/V1.cbz", HAYAI).unwrap_err();
    assert!(err.contains("no longer needs an upgrade"), "{err}");
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        UNKNOWN
    );
    assert_eq!(
        sidecars(&env.lib("A")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"]
    );
}

/// A WebDAV PUT of the main sidecar while its upgrade is in flight is refused (the
/// swap holds the write lock); one between the judge and the swap, or a write that
/// bypasses the lock, makes the swap skip: only the version judged is ever replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_during_an_upgrade_is_refused_as_locked_or_makes_the_swap_skip() {
    let env = Env::new();
    env.add_volume("A").await;
    env.bunko_layer("A/V1.hayai-nova.mokuro");

    // 1. During the swap: 423, and the swap goes through.
    let refused = Arc::new(parking_lot::Mutex::new(None));
    let (app, handle, r) = (
        env.app.clone(),
        tokio::runtime::Handle::current(),
        refused.clone(),
    );
    env.up().set_swap_hook(Some(Arc::new(move |_bare: &Path| {
        let status = handle.block_on(put_via(
            &app,
            "/mokuro-reader/A/V1.mokuro",
            CORRECTED.as_bytes(),
        ));
        *r.lock() = Some(status);
    })));
    let Verdict::Ready(layer) = env.judge("A/V1.cbz") else {
        panic!("ready");
    };
    let up = env.up().clone();
    let cbz = env.lib("A/V1.cbz");
    tokio::task::spawn_blocking(move || up.direct_replace(&cbz, &layer))
        .await
        .unwrap()
        .unwrap();
    env.up().set_swap_hook(None);
    assert_eq!(*refused.lock(), Some(StatusCode::LOCKED));
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    assert_eq!(sidecars(&env.lib("A")), ["V1.mokuro"]);

    // 2. Between the judge and the swap: the swap skips, the PUT stands.
    env.add_volume("B").await;
    env.bunko_layer("B/V1.hayai-nova.mokuro");
    let Verdict::Ready(layer) = env.judge("B/V1.cbz") else {
        panic!("ready");
    };
    assert_eq!(
        env.put("/mokuro-reader/B/V1.mokuro", CORRECTED.as_bytes())
            .await,
        204
    );
    let err = env
        .up()
        .direct_replace(&env.lib("B/V1.cbz"), &layer)
        .unwrap_err();
    assert!(err.contains("changed since it was judged"), "{err}");
    assert_eq!(
        std::fs::read_to_string(env.lib("B/V1.mokuro")).unwrap(),
        CORRECTED
    );
    assert_eq!(
        sidecars(&env.lib("B")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"]
    );
    // The next census upgrades what the PUT left.
    env.upgrade_direct("B/V1.cbz").unwrap();
    assert_eq!(sidecars(&env.lib("B")), ["V1.mokuro"]);

    // 3. A write that bypasses the lock: the re-check of the file's stat just before
    // the rename skips the swap; the next census judges the newer file and upgrades it.
    env.add_volume("C").await;
    env.bunko_layer("C/V1.hayai-nova.mokuro");
    env.up().set_swap_hook(Some(Arc::new(|bare: &Path| {
        std::fs::write(bare, CORRECTED).unwrap();
    })));
    let err = env.upgrade_direct("C/V1.cbz").unwrap_err();
    env.up().set_swap_hook(None);
    assert!(err.contains("changed during its upgrade"), "{err}");
    assert_eq!(
        std::fs::read_to_string(env.lib("C/V1.mokuro")).unwrap(),
        CORRECTED
    );
    env.upgrade_direct("C/V1.cbz").unwrap();
    assert_eq!(sidecars(&env.lib("C")), ["V1.mokuro"]);
    assert_eq!(
        std::fs::read_to_string(env.lib("C/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
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

/// The audit's spellings are resolved through the listings, the same on every host: a
/// folder respelled outside Bunko since (`d/` in the audit, `D/` listed) is found, and a
/// folder that is a hidden case variant (`d/` beside `D/`, where the disk lists both)
/// is a frozen group: nothing in it is removed.
#[tokio::test]
async fn layers_beta3_kept_are_found_by_the_listings() {
    let env = Env::new();
    env.mkcol("D");
    std::fs::write(env.lib("D/V1.cbz"), cbz(3)).unwrap();
    std::fs::write(env.lib("D/V1.mokuro"), HAYAI).unwrap();
    std::fs::write(env.lib("D/V1.mokuro.mokuro"), KEPT).unwrap();
    beta3_upgrade_audit(&env, "d/V1.mokuro", "d/V1.mokuro.mokuro");
    if std::fs::create_dir(env.lib("d")).is_err() {
        // One folder (a disk that folds names): the audit's `d/` is `D/`.
        assert_eq!(env.up().sweep_kept_from_audit(), 1);
        assert_eq!(sidecars(&env.lib("D")), ["V1.mokuro"]);
        return;
    }
    std::fs::write(env.lib("d/V1.cbz"), cbz(3)).unwrap();
    std::fs::write(env.lib("d/V1.mokuro"), HAYAI).unwrap();
    std::fs::write(env.lib("d/V1.mokuro.mokuro"), KEPT).unwrap();
    assert_eq!(env.up().sweep_kept_from_audit(), 0);
    assert_eq!(sidecars(&env.lib("d")), ["V1.mokuro", "V1.mokuro.mokuro"]);
    assert_eq!(sidecars(&env.lib("D")), ["V1.mokuro", "V1.mokuro.mokuro"]);
}

/// `content` sealed, then marked EDITED by a reader (the `edited` block is outside the
/// seal, so the seal still checks).
fn sealed_then_edited(content: &str) -> String {
    let mut v: Value = serde_json::from_str(&sealed(content)).unwrap();
    v["edited"] = serde_json::json!({"by": "mokuro-reader", "at": "2026-10-29T00:00:00Z",
                                     "base_sha256": null, "base_seal": null});
    v.to_string()
}

/// An EDITED main sidecar (an `edited` block, plus a stamp and a seal that check) is
/// never swapped and never copied, whatever its family; the census counts it. An
/// EDITED layer that carries the primary's stamp is never swapped in.
#[tokio::test]
async fn an_edited_main_sidecar_is_never_replaced_and_an_edited_layer_never_swapped_in() {
    let env = Env::new();
    let main = sealed_then_edited(BUNKO_OLD);
    assert!(bunko_layout::seal::is_sealed(main.as_bytes()));
    env.volume_on_disk("A", &main);
    env.bunko_layer("A/V1.hayai-nova.mokuro");
    assert_eq!(env.judge("A/V1.cbz"), Verdict::Edited);
    assert_eq!(env.up().census()["edited"], 1);
    let err = env.upgrade_generated("A/V1.cbz", HAYAI).unwrap_err();
    assert!(err.contains("no longer needs an upgrade"), "{err}");
    let err = env
        .up()
        .direct_replace(&env.lib("A/V1.cbz"), &env.lib("A/V1.hayai-nova.mokuro"))
        .unwrap_err();
    assert!(!err.is_empty());
    assert_eq!(
        std::fs::read_to_string(env.lib("A/V1.mokuro")).unwrap(),
        main
    );
    assert_eq!(
        sidecars(&env.lib("A")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"],
        "nothing written beside it"
    );

    env.volume_on_disk("B", LEGACY);
    std::fs::write(env.lib("B/V1.hayai-nova.mokuro"), sealed_then_edited(HAYAI)).unwrap();
    env.row("B/V1.hayai-nova.mokuro", "hayai-nova", "ppocr-manga");
    assert_eq!(env.judge("B/V1.cbz"), Verdict::NeedsOcr);
}
