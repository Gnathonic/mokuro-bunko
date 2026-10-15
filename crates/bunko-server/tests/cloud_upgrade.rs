//! The generation upgrade on a cloud library (D4, the in-memory `FakeBackend`): the same
//! rules as on a local one (`ocr_upgrade_edited.rs`). An outdated main sidecar Bunko did
//! not stamp is kept as `<Vol>.pre-upgrade.mokuro`, which commits before the swap
//! uploads; a failed layer upload skips the volume until the next census; the node a
//! swap replaces is never deleted for good; a reader's overwrite of a Bunko-written
//! main sidecar forgets its row, so the next census upgrades it after keeping it; one
//! proven to be Bunko's own bytes (its version in the store's ledger, its sha256 in its
//! row) is replaced with no copy, while one that only carries Bunko's stamp is kept
//! first; a layer is swapped in only on the same proof, and unstamped layers are never
//! swapped in or deleted.

// Fixtures are checked with std::fs on purpose (none here touch the cloud through it).
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_core::config::LibraryBackend;
use bunko_db::OcrSidecar;
use bunko_remote::backend::BErr;
use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_server::app::{ServeOptions, Services};
use bunko_server::ocr::upgrade::{Upgrade, Verdict};
use bunko_store::{Durability, Mode, Origin, WriteIntent};
use serde_json::Value;
use sha2::Digest;

const ROOT: &str = "mokuro-reader";

const LEGACY: &str = r#"{"version": "0.2.1", "title": "S", "volume": "Vol 1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;
const HAYAI: &str = r#"{"version":"0.2.5","title":"S","volume":"Vol 1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
const HAYAI_UNSTAMPED: &str = r#"{"version":"0.2.5","title":"S","volume":"Vol 1","volume_uuid":"vu","pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
const BUNKO_OLD: &str = r#"{"version":"0.2.1","title":"S","volume":"Vol 1","ocr_engine":{"id":"mokuro","generator":"mokuro-bunko 0.5.3"},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;

fn cbz(pages: usize) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for i in 0..pages {
        z.start_file(
            format!("{:03}.jpg", i + 1),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(b"fake image bytes").unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// `content` as Bunko's OCR install leaves it: sealed (D4's proof, carried by the file).
fn sealed(content: &str) -> String {
    let mut v = bunko_layout::json::Value::parse(content).unwrap();
    bunko_layout::seal::seal(&mut v).expect("an ocr_engine block to seal")
}

fn no_lease(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.retry_base = Duration::from_millis(50);
}

struct Cloud {
    dir: tempfile::TempDir,
    services: Services,
    fake: Arc<FakeBackend>,
}

/// `f` on a blocking thread (the store blocks on its own runtime).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

impl Cloud {
    /// A server over `fake`, with the reader's files already in it, after its first sync.
    async fn start(fake: Arc<FakeBackend>) -> Cloud {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(no_lease));
        let mut c = Config::default();
        c.storage.base_path = base;
        c.storage.library.backend = LibraryBackend::GDrive;
        c.server.serve_readers = false;
        c.ocr.autobench = false;
        c.catalog.enrich_community = false;
        c.ocr.upgrade.enabled = true;
        c.ocr.upgrade.replace = vec!["mokuro-legacy".into(), "mokuro".into()];
        let services = Services::new(c, None, &ServeOptions::default()).unwrap();
        let storage = services.storage.clone().expect("a cloud library");
        assert!(storage.connected(), "{:?}", storage.problem());
        assert!(
            blocking(move || storage.wait_steady(Duration::from_secs(20))).await,
            "the first sync finishes"
        );
        Cloud {
            dir,
            services,
            fake,
        }
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.services.core.layout.library().join(rel)
    }

    fn up(&self) -> Arc<Upgrade> {
        self.services.ocr.upgrade().clone()
    }

    fn store(&self) -> &Arc<bunko_remote::RemoteStore<FakeBackend>> {
        self.services
            .storage
            .as_ref()
            .unwrap()
            .fake_store()
            .unwrap()
    }

    fn put(&self, rel: &str, bytes: &str) {
        self.fake
            .reader_put(&format!("{ROOT}/{rel}"), bytes.as_bytes());
    }

    fn cloud_bytes(&self, rel: &str) -> Option<String> {
        let n = self.fake.node_at(&format!("{ROOT}/{rel}"))?;
        Some(String::from_utf8(self.fake.content(&n.pid)?).unwrap())
    }

    fn cloud_json(&self, rel: &str) -> Option<Value> {
        serde_json::from_str(&self.cloud_bytes(rel)?).ok()
    }

    /// Pick up the reader's changes now.
    async fn sync(&self) {
        self.store().feed().poll_now();
        let store = self.store().clone();
        assert!(blocking(move || store.wait_steady(Duration::from_secs(20))).await);
    }

    /// The census verdict, judged afresh.
    async fn judge(&self, rel: &str) -> Verdict {
        let (up, cbz, rel) = (self.up(), self.lib(rel), rel.to_string());
        blocking(move || {
            let (config, rows) = (up.config.lock().clone(), up.rows.lock().clone());
            up.configure(&config, &rows);
            up.judge(&cbz, &rel).1
        })
        .await
    }

    async fn upgrade_direct(&self, rel: &str) -> Result<PathBuf, String> {
        let Verdict::Ready(layer) = self.judge(rel).await else {
            panic!("{rel}: a matching layer is ready");
        };
        let (up, cbz) = (self.up(), self.lib(rel));
        blocking(move || up.direct_replace(&cbz, &layer)).await
    }

    async fn upgrade_generated(&self, rel: &str, result: &str) -> Result<PathBuf, String> {
        let out = self
            .dir
            .path()
            .join(format!("result-{}.mokuro", sha(rel.as_bytes())));
        std::fs::write(&out, result).unwrap();
        let (up, cbz) = (self.up(), self.lib(rel));
        blocking(move || up.swap(&cbz, &out, "generated")).await
    }

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

    /// `bytes` published at `rel` by Bunko itself, sealed as an OCR install seals them
    /// (D4's proof), with the row an OCR install records.
    async fn bunko_put(&self, rel: &str, bytes: &'static str, engine: &str, detector: &str) {
        let dest = self.lib(rel);
        let content = sealed(bytes);
        blocking(move || {
            let tmp = dest.with_file_name(format!(
                ".{}.part",
                dest.file_name().unwrap().to_string_lossy()
            ));
            let mut f = bunko_store::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .unwrap();
            f.write_all(content.as_bytes()).unwrap();
            drop(f);
            let intent = WriteIntent::new(
                Mode::CreateNew,
                Origin::Ocr {
                    claim: "c-1".into(),
                },
                Durability::Sync,
            );
            bunko_store::fs::publish(&tmp, &dest, &intent).unwrap();
        })
        .await;
        self.proof(rel, bytes, engine, detector);
    }

    /// The row an OCR install records at `rel` (whoever wrote the file). A row is no
    /// proof: only the file's seal is.
    fn proof(&self, rel: &str, _bytes: &str, engine: &str, detector: &str) {
        self.services
            .db
            .record_ocr_sidecar(&OcrSidecar {
                sidecar_path: rel.into(),
                volume_key: format!("{}.cbz", rel.split('.').next().unwrap()),
                generation_id: "g-1".into(),
                generation_name: engine.into(),
                machine: "local".into(),
                engine: Some(engine.into()),
                detector: Some(detector.into()),
                ..Default::default()
            })
            .unwrap();
    }

    async fn bunko_layer(&self, rel: &str) {
        self.bunko_put(rel, HAYAI, "hayai-nova", "ppocr-manga")
            .await;
    }
}

/// A `pre-upgrade` layer keeps exactly `old` plus its `preserved` block, no stamp.
fn assert_preserves(kept: Option<Value>, old: &str) {
    let mut kept = kept.expect("the pre-upgrade layer is in the cloud");
    let block = kept
        .as_object_mut()
        .unwrap()
        .shift_remove("preserved")
        .expect("a preserved block");
    assert_eq!(block["by"], "mokuro-bunko");
    assert_eq!(block["reason"], "generation-upgrade");
    assert_eq!(block["sha256"], sha(old.as_bytes()));
    assert_eq!(block["replaced_by"], "hayai-nova");
    assert!(kept.get("ocr_engine").is_none(), "{kept}");
    let mut want: Value = serde_json::from_str(old).unwrap();
    want.as_object_mut().unwrap().shift_remove("ocr_engine");
    assert_eq!(kept, want, "the old content, unchanged");
}

fn library(fake: &FakeBackend, main: &str) {
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(3));
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.mokuro"), main.as_bytes());
}

/// MEGA-like: a replace is a new node, and the old one is never deleted for good; the
/// layer upload comes before the swap's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stamp_less_cloud_sidecar_is_kept_then_replaced_and_never_deleted_for_good() {
    let fake = Arc::new(FakeBackend::mega_like());
    library(&fake, LEGACY);
    let cloud = Cloud::start(fake.clone()).await;
    cloud.bunko_layer("Series/Vol 1.hayai-nova.mokuro").await;
    let old = fake
        .node_at(&format!("{ROOT}/Series/Vol 1.mokuro"))
        .unwrap();
    cloud.upgrade_direct("Series/Vol 1.cbz").await.unwrap();

    assert_eq!(
        cloud.cloud_bytes("Series/Vol 1.mokuro").as_deref(),
        Some(sealed(HAYAI).as_str())
    );
    assert_preserves(cloud.cloud_json("Series/Vol 1.pre-upgrade.mokuro"), LEGACY);
    let uploads = fake.upload_log();
    let at = |name: &str| uploads.iter().position(|n| n == name);
    assert!(
        at("Vol 1.pre-upgrade.mokuro").unwrap() < at("Vol 1.mokuro").unwrap(),
        "the layer committed before the swap uploaded: {uploads:?}"
    );
    let store = cloud.store().clone();
    assert!(blocking(move || store.wait_outbox_idle(Duration::from_secs(20))).await);
    // The old node is no longer live, and was never deleted for good. (The fake's MEGA
    // replace drops the old node itself, so SUPERSEDE_OLD finds nothing left to trash
    // here; when it does find one, an `Upgrade` swap always trashes it.)
    assert!(
        fake.node_at(&format!("{ROOT}/Series/Vol 1.mokuro"))
            .is_some_and(|n| n.pid != old.pid)
    );
    assert!(
        !fake.deleted_log().contains(&old.pid),
        "never deleted for good"
    );
    assert_eq!(
        cloud.upgrade_audits()[0]["preserved_as"],
        "Series/Vol 1.pre-upgrade.mokuro"
    );
    assert!(
        cloud
            .services
            .db
            .get_ocr_sidecar("Series/Vol 1.pre-upgrade.mokuro")
            .unwrap()
            .is_none()
    );
    assert_eq!(cloud.judge("Series/Vol 1.cbz").await, Verdict::Current);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_layer_upload_skips_the_cloud_volume_until_the_next_census() {
    let fake = Arc::new(FakeBackend::drive_like());
    library(&fake, LEGACY);
    let cloud = Cloud::start(fake.clone()).await;
    cloud.bunko_layer("Series/Vol 1.hayai-nova.mokuro").await;
    fake.fail_next(
        FakeOp::Upload,
        BErr::Fatal("the provider refused it".into()),
    );
    let err = cloud.upgrade_direct("Series/Vol 1.cbz").await.unwrap_err();
    assert!(err.contains("pre-upgrade"), "{err}");
    assert_eq!(
        cloud.cloud_bytes("Series/Vol 1.mokuro").as_deref(),
        Some(LEGACY)
    );
    assert!(
        cloud
            .cloud_bytes("Series/Vol 1.pre-upgrade.mokuro")
            .is_none()
    );
    assert!(cloud.upgrade_audits().is_empty());

    cloud.upgrade_direct("Series/Vol 1.cbz").await.unwrap();
    assert_eq!(
        cloud.cloud_bytes("Series/Vol 1.mokuro").as_deref(),
        Some(sealed(HAYAI).as_str())
    );
    assert_preserves(cloud.cloud_json("Series/Vol 1.pre-upgrade.mokuro"), LEGACY);
}

/// Bunko's own `<Vol>.mokuro`, then the reader saves a correction over it in the
/// cloud: the row no longer describes the file, so the census sees the old recipe and
/// upgrades it, keeping the reader's bytes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_overwrite_in_the_cloud_forgets_the_row_and_is_kept_before_the_upgrade() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(3));
    let cloud = Cloud::start(fake.clone()).await;
    // Bunko writes the main sidecar and records it, as an OCR install does.
    let bare = cloud.lib("Series/Vol 1.mokuro");
    blocking(move || {
        let tmp = bare.with_file_name(".Vol 1.mokuro.part");
        let mut f = bunko_store::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .unwrap();
        f.write_all(HAYAI.as_bytes()).unwrap();
        drop(f);
        let intent = WriteIntent::new(
            Mode::CreateNew,
            Origin::Ocr {
                claim: "c-1".into(),
            },
            Durability::Sync,
        );
        bunko_store::fs::publish(&tmp, &bare, &intent).unwrap();
    })
    .await;
    cloud.proof("Series/Vol 1.mokuro", HAYAI, "hayai-nova", "ppocr-manga");
    assert_eq!(cloud.judge("Series/Vol 1.cbz").await, Verdict::Current);

    // The reader re-uploads it from its DB: no stamp.
    cloud.put("Series/Vol 1.mokuro", LEGACY);
    cloud.sync().await;
    let db = cloud.services.db.clone();
    let until = Instant::now() + Duration::from_secs(10);
    while db.get_ocr_sidecar("Series/Vol 1.mokuro").unwrap().is_some() {
        assert!(Instant::now() < until, "the overwrite forgets the row");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(cloud.judge("Series/Vol 1.cbz").await, Verdict::NeedsOcr);
    cloud
        .upgrade_generated("Series/Vol 1.cbz", HAYAI)
        .await
        .unwrap();
    assert_eq!(
        cloud.cloud_bytes("Series/Vol 1.mokuro").as_deref(),
        Some(HAYAI)
    );
    assert_preserves(cloud.cloud_json("Series/Vol 1.pre-upgrade.mokuro"), LEGACY);
}

/// Bunko's own main sidecar (its version in the ledger, its bytes in its row) is replaced
/// with no copy; a stamped one a reader uploaded is kept first, as is one whose row
/// matches but whose version Bunko did not write; reader-uploaded layers, stamped or
/// not, are never swapped in or deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cloud_sidecar_proven_bunkos_own_is_replaced_without_a_copy_and_others_are_kept() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(3));
    // Vol 2: a reader pushed the hayai-nova layer back without its stamp; its row
    // still names hayai-nova. A person's own layer, with no row, beside it.
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.cbz"), &cbz(3));
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.mokuro"), LEGACY.as_bytes());
    fake.reader_put(
        &format!("{ROOT}/Series/Vol 2.hayai-nova.mokuro"),
        HAYAI_UNSTAMPED.as_bytes(),
    );
    fake.reader_put(
        &format!("{ROOT}/Series/Vol 2.mine.mokuro"),
        HAYAI_UNSTAMPED.as_bytes(),
    );
    // Vol 3: a reader's upload carrying Bunko's stamp, beside a layer a reader uploaded
    // with the matching stamp. Vol 4: the same stamped main, with a row whose sha256
    // matches it, but the version is not one Bunko wrote.
    for v in ["Vol 3", "Vol 4"] {
        fake.reader_put(&format!("{ROOT}/Series/{v}.cbz"), &cbz(3));
        fake.reader_put(&format!("{ROOT}/Series/{v}.mokuro"), BUNKO_OLD.as_bytes());
    }
    fake.reader_put(
        &format!("{ROOT}/Series/Vol 3.hayai-nova.mokuro"),
        HAYAI.as_bytes(),
    );
    let cloud = Cloud::start(fake.clone()).await;
    cloud
        .bunko_put("Series/Vol 1.mokuro", BUNKO_OLD, "mokuro", "ctd")
        .await;
    cloud.bunko_layer("Series/Vol 1.hayai-nova.mokuro").await;

    cloud.upgrade_direct("Series/Vol 1.cbz").await.unwrap();
    assert_eq!(
        cloud.cloud_bytes("Series/Vol 1.mokuro").as_deref(),
        Some(sealed(HAYAI).as_str())
    );
    assert!(
        cloud
            .cloud_bytes("Series/Vol 1.pre-upgrade.mokuro")
            .is_none()
    );
    assert!(cloud.upgrade_audits()[0].get("preserved_as").is_none());

    // Vol 3: the reader-uploaded stamped layer is no source; a job's result replaces
    // the stamped main only after keeping it.
    assert_eq!(cloud.judge("Series/Vol 3.cbz").await, Verdict::NeedsOcr);
    let (up, cbz3, layer3) = (
        cloud.up(),
        cloud.lib("Series/Vol 3.cbz"),
        cloud.lib("Series/Vol 3.hayai-nova.mokuro"),
    );
    let err = blocking(move || up.direct_replace(&cbz3, &layer3))
        .await
        .unwrap_err();
    assert!(err.contains("not proven"), "{err}");
    cloud
        .upgrade_generated("Series/Vol 3.cbz", HAYAI)
        .await
        .unwrap();
    assert_preserves(
        cloud.cloud_json("Series/Vol 3.pre-upgrade.mokuro"),
        BUNKO_OLD,
    );
    assert_eq!(
        cloud
            .cloud_bytes("Series/Vol 3.hayai-nova.mokuro")
            .as_deref(),
        Some(HAYAI),
        "never deleted"
    );

    // Vol 4: the row alone is no proof: only the file's seal is.
    cloud.proof("Series/Vol 4.mokuro", BUNKO_OLD, "mokuro", "ctd");
    assert_eq!(cloud.judge("Series/Vol 4.cbz").await, Verdict::NeedsOcr);
    cloud
        .upgrade_generated("Series/Vol 4.cbz", HAYAI)
        .await
        .unwrap();
    assert_preserves(
        cloud.cloud_json("Series/Vol 4.pre-upgrade.mokuro"),
        BUNKO_OLD,
    );

    cloud
        .services
        .db
        .record_ocr_sidecar(&OcrSidecar {
            sidecar_path: "Series/Vol 2.hayai-nova.mokuro".into(),
            volume_key: "Series/Vol 2.cbz".into(),
            generation_id: "g-2".into(),
            generation_name: "hayai-nova".into(),
            machine: "local".into(),
            engine: Some("hayai-nova".into()),
            detector: Some("ppocr-manga".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(cloud.judge("Series/Vol 2.cbz").await, Verdict::NeedsOcr);
    let (up, cbz2, layer) = (
        cloud.up(),
        cloud.lib("Series/Vol 2.cbz"),
        cloud.lib("Series/Vol 2.hayai-nova.mokuro"),
    );
    let err = blocking(move || up.direct_replace(&cbz2, &layer))
        .await
        .unwrap_err();
    assert!(err.contains("not proven"), "{err}");
    let trashed = fake.trashed_log().len();
    cloud
        .upgrade_generated("Series/Vol 2.cbz", HAYAI)
        .await
        .unwrap();
    assert_preserves(cloud.cloud_json("Series/Vol 2.pre-upgrade.mokuro"), LEGACY);
    for layer in ["Series/Vol 2.hayai-nova.mokuro", "Series/Vol 2.mine.mokuro"] {
        assert_eq!(
            cloud.cloud_bytes(layer).as_deref(),
            Some(HAYAI_UNSTAMPED),
            "{layer} untouched"
        );
    }
    assert_eq!(cloud.judge("Series/Vol 2.cbz").await, Verdict::Current);
    assert_eq!(fake.trashed_log().len(), trashed, "nothing trashed");
}
