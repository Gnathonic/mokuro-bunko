//! Generation upgrade (spec generation-upgrade.md): census, direct replace, generate then
//! swap through the scheduler. The upgrade replaces the old file and keeps no layer of it;
//! layers an earlier beta kept are cleaned up.

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
    let mut h = harness_full(vec![primary()], Arc::new(FileFacts), Some(enabled()));
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("A/V1.hayai-nova.mokuro"), HAYAI).unwrap();
    h.at(0.0);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        HAYAI,
        "the layer's bytes are the new bare file"
    );
    assert_eq!(
        sidecars(&lib.join("A")),
        ["V1.mokuro"],
        "the old file is replaced, not kept; the layer it came from is not kept twice"
    );
    assert!(
        !h.s.owed.volumes.contains_key("A/V1.cbz"),
        "nothing left to do"
    );
}

#[test]
fn a_direct_replace_keeps_a_layer_an_enabled_row_writes() {
    let mut h = harness_full(
        vec![primary(), layer("g-2", "nova")],
        Arc::new(FileFacts),
        Some(enabled()),
    );
    h.add("A/V1.cbz", 3);
    let lib = h.library();
    std::fs::write(lib.join("A/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("A/V1.nova.mokuro"), HAYAI).unwrap();
    h.at(0.0);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        HAYAI
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
    // Swapped in, never `_1`-suffixed; the old file replaced, no layer kept.
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
