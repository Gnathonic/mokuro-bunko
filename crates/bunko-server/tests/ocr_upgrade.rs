//! Generation upgrade (spec generation-upgrade.md): census, direct replace, generate then
//! swap through the scheduler. The upgrade replaces the old file outright, with no copy
//! kept, whoever wrote it (D4, `ocr_upgrade_edited.rs`); layers an earlier beta kept are
//! cleaned up.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::sync::Arc;

use bunko_core::config::UpgradeConfig;
use bunko_proto::Op;
use bunko_server::ocr::types::{FileFacts, Job, LibraryFacts};
use ocr_common::*;

const LEGACY: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;
const HAYAI: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;

/// The old file as 0.7.0-beta.3's upgrade kept it beside the new one.
const KEPT: &str = r#"{"version":"0.2.1","title":"A","volume":"V1","pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}],"ocr_engine":{"id":"mokuro","generation":"mokuro","generator":"mokuro-bunko 0.7.0-beta.3","upgraded_from_primary":true}}"#;

fn enabled() -> UpgradeConfig {
    UpgradeConfig {
        enabled: true,
        replace: vec!["mokuro-legacy".into(), "mokuro".into()],
    }
}

fn json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
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

#[test]
fn direct_replace_swaps_a_matching_layer_in() {
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    h.bunko_wrote("A/V1.hayai-nova.mokuro", HAYAI, &layer("g-2", "hayai-nova"));
    h.at(0.0);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        sealed(HAYAI),
        "the layer's bytes are the new bare file"
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.mokuro"],
        "no copy of the old file; the layer it came from is not kept twice"
    );
    assert!(
        !h.s.owed.volumes.contains_key("A/V1.cbz"),
        "nothing left to do"
    );
    assert!(
        bunko_layout::seal::is_sealed(&std::fs::read(lib.join("A/V1.mokuro")).unwrap()),
        "the proof moved with the bytes"
    );
}

/// An OCR install seals what it put in place (`ocr_engine.seal`).
#[test]
fn an_install_seals_the_bytes_it_wrote() {
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("B/V9.cbz", 3);
    let lib = h.library();
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[0].1, 3));
    h.done_with(&p, &sid, &vols[0].1, "V9.mokuro", 3, 1.0, HAYAI.as_bytes());
    let installed = std::fs::read(lib.join("B/V9.mokuro")).unwrap();
    assert!(
        bunko_layout::seal::is_sealed(&installed),
        "the normalised bytes on disk are sealed"
    );
    assert!(
        !bunko_layout::seal::is_sealed(HAYAI.as_bytes()),
        "the processor's bytes were not"
    );
}

/// Critique C11: a layer's seal is taken after normalisation stamps `generation` into
/// its block, and it still checks once that layer is promoted to the main sidecar.
#[test]
fn a_layer_is_sealed_after_its_stamp_and_stays_sealed_as_the_main_sidecar() {
    let nova = layer("g-2", "nova");
    // A layer row's install: the processor's bytes, normalised, stamped, sealed.
    let mut h = harness_db(vec![primary(), nova.clone()], Arc::new(FileFacts), None);
    h.add("A/V1.cbz", 3);
    std::fs::write(h.library().join("A/V1.mokuro"), LEGACY).unwrap();
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[0].1, 3));
    h.done_with(
        &p,
        &sid,
        &vols[0].1,
        "V1.nova.mokuro",
        3,
        1.0,
        HAYAI.as_bytes(),
    );
    let installed = std::fs::read_to_string(h.library().join("A/V1.nova.mokuro")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&installed).unwrap();
    assert_eq!(v["ocr_engine"]["generation"], "nova");
    assert!(bunko_layout::seal::is_sealed(installed.as_bytes()));
    let unstamped: serde_json::Value = serde_json::from_str(&sealed(HAYAI)).unwrap();
    assert_ne!(
        v["ocr_engine"]["seal"], unstamped["ocr_engine"]["seal"],
        "the seal covers the stamped block, not the processor's"
    );

    // The same file on a library whose primary it now matches: promoted as it is.
    let mut h = harness_db(
        vec![primary(), nova.clone()],
        Arc::new(FileFacts),
        Some(enabled()),
    );
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    h.bunko_wrote("A/V1.nova.mokuro", &installed, &nova);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.nova.mokuro")).unwrap(),
        installed,
        "sealing a sealed file again changes nothing"
    );
    h.at(0.0);
    let main = std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap();
    assert_eq!(
        main, installed,
        "the layer's bytes are the new main sidecar"
    );
    assert!(
        bunko_layout::seal::is_sealed(main.as_bytes()),
        "the promoted main is sealed"
    );
}

/// The same layer with no proof that Bunko wrote it (no seal): its stamp alone does not make it the main sidecar, so the volume needs a job.
#[test]
fn a_stamped_layer_without_proof_is_not_swapped_in() {
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("A/V1.hayai-nova.mokuro"), HAYAI).unwrap();
    h.at(0.0);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        LEGACY
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"]
    );
    assert!(
        h.s.owed.volumes["A/V1.cbz"].upgrade,
        "an upgrade job instead"
    );
}

#[test]
fn a_direct_replace_keeps_a_layer_an_enabled_row_writes() {
    let nova = layer("g-2", "nova");
    let mut h = harness_db(
        vec![primary(), nova.clone()],
        Arc::new(FileFacts),
        Some(enabled()),
    );
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    h.bunko_wrote("A/V1.nova.mokuro", HAYAI, &nova);
    h.at(0.0);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        sealed(HAYAI)
    );
    // Removing it would only queue the `nova` row to write it again.
    assert_eq!(sidecars(&lib.join("A")), ["V1.mokuro", "V1.nova.mokuro"]);
}

#[test]
fn a_volume_without_a_match_gets_an_upgrade_job_after_ordinary_work() {
    let mut h = harness_full(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    h.add("B/V9.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    h.at(0.0);
    assert!(h.s.owed.volumes["A/V1.cbz"].upgrade);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    // A volume with no OCR beats one with old OCR.
    assert_eq!(vols[0].2, "/mokuro-reader/B/V9.cbz");
    assert_eq!(vols[1].2, "/mokuro-reader/A/V1.cbz");
    assert!(h.s.claims.contains_key(&Job::upgrade("A/V1.cbz", "g-1")));
    let name = ops
        .iter()
        .filter_map(|o| {
            if let Op::Volume(v) = o {
                Some(v.sidecar_name.clone())
            } else {
                None
            }
        })
        .nth(1)
        .unwrap();
    assert_eq!(name, "V1.mokuro");
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[1].1, 3));
    h.done_with(&p, &sid, &vols[1].1, &name, 3, 1.0, HAYAI.as_bytes());
    // Swapped in, never `_1`-suffixed; no copy of the old file (no Bunko stamp).
    let bare = json(&lib.join("A/V1.mokuro"));
    assert_eq!(bare["ocr_engine"]["id"], "hayai-nova");
    assert_eq!(sidecars(&lib.join("A")), ["V1.mokuro"]);
    assert!(h.s.failures.is_empty());
    // The next walk sees the current recipe: nothing more.
    h.at(31.0);
    assert!(h.s.owed.volumes.get("A/V1.cbz").is_none_or(|v| !v.upgrade));
}

struct Short;
impl LibraryFacts for Short {
    fn missing_pages(&self, _cbz: &std::path::Path) -> i64 {
        2
    }
}

#[test]
fn short_archives_and_disabled_policy_are_left_alone() {
    let mut h = harness_full(vec![primary()], Arc::new(Short), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    h.at(0.0);
    assert!(!h.s.owed.volumes.contains_key("A/V1.cbz"));
    let census = h.s.deps.upgrade.clone().unwrap().census();
    assert_eq!(census["skipped_missing_pages"], 1);
    assert_eq!(census["families"]["mokuro-legacy"], 1);

    let mut off = harness_full(
        vec![primary()],
        Arc::new(FileFacts),
        Some(UpgradeConfig::default()),
    );
    off.add("A/V1.cbz", 3);
    std::fs::write(off.library().join("A/V1.mokuro"), LEGACY).unwrap();
    off.at(0.0);
    assert!(
        !off.s.owed.volumes.contains_key("A/V1.cbz"),
        "upgrades are off by default"
    );
}

#[test]
fn a_layer_an_earlier_beta_kept_goes_once_the_volume_is_upgraded() {
    let mut h = harness_full(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    let lib = h.library();
    // A: upgraded by beta.3, which kept the old file as a stamped layer.
    std::fs::write(lib.join("A/V1.mokuro"), HAYAI).unwrap();
    std::fs::write(lib.join("A/V1.mokuro.mokuro"), KEPT).unwrap();
    // Not stamped: what a forced upgrade kept of a person's edit, and a person's layer.
    std::fs::write(lib.join("A/V1.mokuro-prev.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("A/V1.mokuro-old.mokuro"), LEGACY).unwrap();
    // B: a crash left the stamped copy beside the old file, not yet replaced.
    std::fs::write(lib.join("B/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("B/V1.mokuro.mokuro"), KEPT).unwrap();
    h.at(0.0);
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.mokuro", "V1.mokuro-old.mokuro", "V1.mokuro-prev.mokuro"],
        "only the stamped layer goes"
    );
    assert_eq!(
        sidecars(&lib.join("B")),
        ["V1.mokuro", "V1.mokuro.mokuro"],
        "the bare file is not the upgraded output yet"
    );
    // B is upgraded; the next census removes its stamped copy too.
    assert!(h.s.owed.volumes["B/V1.cbz"].upgrade);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[0].1, 3));
    h.done_with(&p, &sid, &vols[0].1, "V1.mokuro", 3, 1.0, HAYAI.as_bytes());
    assert_eq!(
        json(&lib.join("B/V1.mokuro"))["ocr_engine"]["id"],
        "hayai-nova"
    );
    h.at(31.0);
    assert_eq!(sidecars(&lib.join("B")), ["V1.mokuro"]);
}

/// The harness's upgrade census, configured like the scheduler's.
fn upgrade_of(h: &H) -> Arc<bunko_server::ocr::upgrade::Upgrade> {
    h.s.deps.upgrade.clone().expect("an upgrade census")
}

fn judge(h: &H, rel: &str) -> bunko_server::ocr::upgrade::Verdict {
    upgrade_of(h).judge(&h.library().join(rel), rel).1
}

/// HAYAI at `revision`.
fn hayai_at(revision: u32) -> String {
    HAYAI.replace(
        r#""id":"hayai-nova","#,
        &format!(r#""id":"hayai-nova","revision":{revision},"#),
    )
}

/// A revision bump outdates the primary engine's own output even when its family is not
/// listed under `replace`: Bunko's sealed main is queued and replaced with no copy.
/// Off, nothing is outdated.
#[test]
fn a_revision_bump_upgrades_bunkos_sealed_main_with_no_copy() {
    use bunko_server::ocr::upgrade::Verdict;
    let on = UpgradeConfig {
        enabled: true,
        replace: vec![],
    };
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(on.clone()));
    h.add("A/V1.cbz", 3);
    h.bunko_wrote("A/V1.mokuro", HAYAI, &primary());
    let up = upgrade_of(&h);
    assert_eq!(judge(&h, "A/V1.cbz"), Verdict::Current);
    up.set_engine_revision("hayai-nova", 2);
    assert_eq!(judge(&h, "A/V1.cbz"), Verdict::NeedsOcr);
    let off = UpgradeConfig {
        enabled: false,
        ..on.clone()
    };
    up.configure(&off, &[primary()]);
    assert_eq!(judge(&h, "A/V1.cbz"), Verdict::Current);
    up.configure(&on, &[primary()]);
    h.at(0.0);
    assert!(h.s.owed.volumes["A/V1.cbz"].upgrade);
    let out = h.dir.path().join("result.mokuro");
    std::fs::write(&out, sealed(&hayai_at(2))).unwrap();
    let lib = h.library();
    up.swap(&lib.join("A/V1.cbz"), &out, "generated").unwrap();
    assert_eq!(sidecars(&lib.join("A")), ["V1.mokuro"], "no copy");
    assert_eq!(json(&lib.join("A/V1.mokuro"))["ocr_engine"]["revision"], 2);
    assert_eq!(judge(&h, "A/V1.cbz"), Verdict::Current);
}

/// 0.7.3's main sidecars carry a block with no revision and no seal: revision 1, current
/// against today's registry, so upgrading to 0.8 re-runs nothing.
#[test]
fn a_0_7_main_without_revision_or_seal_is_current() {
    use bunko_server::ocr::upgrade::Verdict;
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    std::fs::write(h.library().join("A/V1.mokuro"), HAYAI).unwrap();
    assert_eq!(judge(&h, "A/V1.cbz"), Verdict::Current);
    h.at(0.0);
    assert!(!h.s.owed.volumes.contains_key("A/V1.cbz"));
}

/// A reader renamed the volume: new title, volume and chars, pretty-printed, the block
/// carried verbatim. Still current and still sealed, so a later revision bump replaces
/// it with no copy.
#[test]
fn a_reader_renamed_main_keeps_its_seal_and_recipe() {
    use bunko_server::ocr::upgrade::Verdict;
    let h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("B/V2.cbz", 3);
    let mut v: serde_json::Value = serde_json::from_str(&sealed(HAYAI)).unwrap();
    v["title"] = "B".into();
    v["volume"] = "V2".into();
    v["chars"] = 1234.into();
    let renamed = serde_json::to_string_pretty(&v).unwrap();
    std::fs::write(h.library().join("B/V2.mokuro"), &renamed).unwrap();
    assert!(bunko_layout::seal::is_sealed(renamed.as_bytes()));
    assert_eq!(judge(&h, "B/V2.cbz"), Verdict::Current);
    upgrade_of(&h).set_engine_revision("hayai-nova", 2);
    assert_eq!(judge(&h, "B/V2.cbz"), Verdict::NeedsOcr);
    let out = h.dir.path().join("result.mokuro");
    std::fs::write(&out, sealed(&hayai_at(2))).unwrap();
    let lib = h.library();
    upgrade_of(&h)
        .swap(&lib.join("B/V2.cbz"), &out, "generated")
        .unwrap();
    assert_eq!(sidecars(&lib.join("B")), ["V2.mokuro"], "no copy");
}

/// Critique C10: with upgrades on, a generation layer Bunko wrote (sealed, its row's
/// engine) at a lower revision than the engine's is owed again, and the new result
/// replaces it in place. A layer that is not Bunko's own (unsealed, edited) is never
/// owed: the result could only be discarded, round after round. Off: nothing is owed.
#[test]
fn a_stale_generation_layer_is_run_again_and_replaced_in_place() {
    let main = sealed(&hayai_at(2));
    let stale = sealed(&hayai_at(1).replace(
        r#""patch_budget":512"#,
        r#""patch_budget":512,"generation":"fast""#,
    ));
    let on = UpgradeConfig {
        enabled: true,
        replace: vec![],
    };
    let rows = vec![primary(), layer("g-2", "fast")];
    // Off: the layer is done.
    let mut off = harness_db(rows.clone(), Arc::new(FileFacts), None);
    off.add("A/V1.cbz", 3);
    std::fs::write(off.library().join("A/V1.mokuro"), &main).unwrap();
    std::fs::write(off.library().join("A/V1.fast.mokuro"), &stale).unwrap();
    off.at(0.0);
    assert!(!off.s.owed.volumes.contains_key("A/V1.cbz"));

    let mut h = harness_db(rows.clone(), Arc::new(FileFacts), Some(on));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), &main).unwrap();
    // Not Bunko's own: never owed. Each check is a fresh walk (one per poll interval).
    let mut t = 0.0;
    for theirs in [
        hayai_at(1),
        serde_json::to_string(&{
            let mut v: serde_json::Value = serde_json::from_str(&stale).unwrap();
            v["edited"] = serde_json::json!({"by": "mokuro-reader"});
            v
        })
        .unwrap(),
    ] {
        std::fs::write(lib.join("A/V1.fast.mokuro"), &theirs).unwrap();
        upgrade_of(&h).set_engine_revision("hayai-nova", 2);
        h.at(t);
        t += 31.0;
        assert!(!h.s.owed.volumes.contains_key("A/V1.cbz"), "{theirs}");
    }
    std::fs::write(lib.join("A/V1.fast.mokuro"), &stale).unwrap();
    h.at(t);
    assert_eq!(
        h.s.owed.volumes["A/V1.cbz"].rows,
        [Arc::<str>::from("g-2")],
        "the stale layer is owed, not the main"
    );
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let (_, claim, _) = volumes(&ops)[0].clone();
    let name = ops
        .iter()
        .find_map(|o| match o {
            Op::Volume(v) => Some(v.sidecar_name.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(name, "V1.fast.mokuro");
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 3));
    h.done_with(&p, &sid, &claim, &name, 3, 1.0, hayai_at(2).as_bytes());
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.fast.mokuro", "V1.mokuro"],
        "replaced in place, no _1"
    );
    assert_eq!(
        json(&lib.join("A/V1.fast.mokuro"))["ocr_engine"]["revision"],
        2
    );
    assert!(h.s.failures.is_empty());
    h.at(t + 31.0);
    assert!(!h.s.owed.volumes.contains_key("A/V1.cbz"), "current now");
}

/// An old recipe Bunko itself wrote (0.5's served mokuro carries its stamp).
const BUNKO_OLD: &str = r#"{"version":"0.2.1","title":"A","volume":"V1","ocr_engine":{"id":"mokuro","generator":"mokuro-bunko 0.5.3"},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;

/// `content` sealed, then marked EDITED by a reader (the `edited` block is outside the
/// seal).
fn sealed_then_edited(content: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&sealed(content)).unwrap();
    v["edited"] = serde_json::json!({"by": "mokuro-reader", "at": "2026-10-29T00:00:00Z",
                                     "base_sha256": null, "base_seal": null});
    v.to_string()
}

/// Critique C4-B6: an EDITED main whose recipe is outdated gets one job; its result
/// lands as the layer `<Vol>.<primary>.mokuro`, stamped and sealed, and the main keeps
/// its bytes. Once the edit mark is gone, that layer is what the census swaps in.
#[test]
fn an_edited_main_gets_its_new_ocr_as_the_primary_layer_and_is_never_touched() {
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    let main = sealed_then_edited(BUNKO_OLD);
    std::fs::write(lib.join("A/V1.mokuro"), &main).unwrap();
    h.at(0.0);
    assert!(h.s.owed.volumes["A/V1.cbz"].upgrade, "one upgrade job");
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    assert_eq!(vols.len(), 1);
    assert!(h.s.claims.contains_key(&Job::upgrade("A/V1.cbz", "g-1")));
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[0].1, 3));
    h.done_with(&p, &sid, &vols[0].1, "V1.mokuro", 3, 1.0, HAYAI.as_bytes());
    assert!(h.s.failures.is_empty(), "{:?}", h.s.failures);

    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        main,
        "the edited main keeps its bytes"
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"],
        "nothing copied, nothing numbered"
    );
    let layer = lib.join("A/V1.hayai-nova.mokuro");
    let block = json(&layer)["ocr_engine"].clone();
    assert_eq!(block["id"], "hayai-nova");
    assert_eq!(block["generation"], "hayai-nova");
    assert!(bunko_layout::seal::is_sealed(
        &std::fs::read(&layer).unwrap()
    ));
    let db = h.db.clone().unwrap();
    let row = db
        .get_ocr_sidecar("A/V1.hayai-nova.mokuro")
        .unwrap()
        .expect("a provenance row at the layer");
    assert_eq!(row.generation_id, "g-1");
    let written: Vec<serde_json::Value> = db
        .list_audit_events(50, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "ocr_sidecar_written")
        .map(|e| serde_json::from_str(e.details.as_deref().unwrap()).unwrap())
        .collect();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["mode"], "edited-main-layer");

    // The next walk owes nothing.
    h.at(31.0);
    assert!(h.s.owed.volumes.get("A/V1.cbz").is_none_or(|v| !v.upgrade));

    // The edit mark gone (Bunko's own old file again): the layer is swapped in.
    std::fs::write(lib.join("A/V1.mokuro"), sealed(BUNKO_OLD)).unwrap();
    h.at(62.0);
    assert_eq!(
        json(&lib.join("A/V1.mokuro"))["ocr_engine"]["id"],
        "hayai-nova"
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.mokuro"],
        "the layer copied in is not kept twice"
    );
}

/// No job for an EDITED main with upgrades off, with a current recipe beneath the edit,
/// with none at all, or when a file it is not Bunko's own holds the layer's name (its
/// result could only be discarded, every walk).
#[test]
fn an_edited_main_gets_no_job_unless_its_recipe_is_outdated_and_the_layer_free() {
    let mut off = harness_db(
        vec![primary()],
        Arc::new(FileFacts),
        Some(UpgradeConfig::default()),
    );
    off.add("A/V1.cbz", 3);
    std::fs::write(
        off.library().join("A/V1.mokuro"),
        sealed_then_edited(BUNKO_OLD),
    )
    .unwrap();
    off.at(0.0);
    assert!(!off.s.owed.volumes.contains_key("A/V1.cbz"), "upgrades off");

    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    let lib = h.library();
    for v in ["A", "B", "C", "D"] {
        h.add(&format!("{v}/V1.cbz"), 3);
    }
    std::fs::write(lib.join("A/V1.mokuro"), sealed_then_edited(HAYAI)).unwrap();
    let mut no_recipe: serde_json::Value = serde_json::from_str(LEGACY).unwrap();
    no_recipe.as_object_mut().unwrap().shift_remove("version");
    no_recipe["edited"] = serde_json::json!({"by": "mokuro-reader"});
    std::fs::write(lib.join("B/V1.mokuro"), no_recipe.to_string()).unwrap();
    std::fs::write(lib.join("C/V1.mokuro"), sealed_then_edited(BUNKO_OLD)).unwrap();
    std::fs::write(lib.join("C/V1.hayai-nova.mokuro"), HAYAI).unwrap();
    std::fs::write(lib.join("D/V1.mokuro"), sealed_then_edited(BUNKO_OLD)).unwrap();
    std::fs::write(lib.join("D/v1.hayai-nova.mokuro.gz"), b"\x1f\x8b").unwrap();
    h.at(0.0);
    for v in ["A", "B", "C", "D"] {
        assert!(
            h.s.owed
                .volumes
                .get(&format!("{v}/V1.cbz"))
                .is_none_or(|o| !o.upgrade),
            "{v}: no job"
        );
    }
    assert_eq!(h.s.deps.upgrade.clone().unwrap().census()["edited"], 4);
}

/// An EDITED main as Mokuro Reader writes it (interop §1): mokuro's `version`, an
/// `edited` block, and no `ocr_engine` (a file never carries both). Nothing in it names
/// the recipe beneath the edit, whatever the base was (the edit's PUT also forgot the
/// row), so it reads as `mokuro-legacy`: listed in the default `replace`, it is owed ONE
/// run of the primary, written beside it as `<Vol>.<primary>.mokuro`. Not listed, it is
/// owed none.
#[test]
fn a_reader_shaped_edited_main_is_owed_one_primary_layer_under_the_default_replace_list() {
    let mut reader_edited: serde_json::Value = serde_json::from_str(LEGACY).unwrap();
    reader_edited["edited"] = serde_json::json!({"by": "mokuro-reader",
        "at": "2026-10-29T00:00:00Z", "base_sha256": "ab", "base_seal": null});
    let main = reader_edited.to_string();

    let mut unlisted = harness_db(
        vec![primary()],
        Arc::new(FileFacts),
        Some(UpgradeConfig {
            enabled: true,
            replace: vec![],
        }),
    );
    unlisted.add("A/V1.cbz", 3);
    std::fs::write(unlisted.library().join("A/V1.mokuro"), &main).unwrap();
    unlisted.at(0.0);
    assert!(
        !unlisted.s.owed.volumes.contains_key("A/V1.cbz"),
        "mokuro-legacy not listed"
    );

    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), &main).unwrap();
    h.at(0.0);
    assert!(h.s.owed.volumes["A/V1.cbz"].upgrade, "one upgrade job");
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let vols = volumes(&ops);
    assert_eq!(vols.len(), 1);
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &vols[0].1, 3));
    h.done_with(&p, &sid, &vols[0].1, "V1.mokuro", 3, 1.0, HAYAI.as_bytes());
    assert!(h.s.failures.is_empty(), "{:?}", h.s.failures);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        main
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.hayai-nova.mokuro", "V1.mokuro"]
    );
    // Once: the layer is there, Bunko's and current.
    h.at(31.0);
    assert!(h.s.owed.volumes.get("A/V1.cbz").is_none_or(|v| !v.upgrade));
    h.at(62.0);
    assert!(h.s.owed.volumes.get("A/V1.cbz").is_none_or(|v| !v.upgrade));
}
