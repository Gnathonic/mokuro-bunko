//! The `_n` sweep (critique C4-B5): `<stem>_<n>.mokuro` files an older collector
//! numbered beside a taken destination, which no reader ever matches. A walk finds them;
//! the sweep after it renames a provably-Bunko orphan to its canonical name when that is
//! free and removes it when it is taken; anything else stays, counted in the status.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::sync::Arc;

use bunko_server::ocr::types::FileFacts;
use ocr_common::*;

const HAYAI: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
const LEGACY: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;

/// The names in a series folder, sorted.
fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

#[test]
fn provably_bunkos_orphans_are_renamed_or_removed_and_others_stay_counted() {
    let mut h = harness_db(
        vec![primary(), layer("g-2", "fast")],
        Arc::new(FileFacts),
        None,
    );
    let lib = h.library();
    for v in ["A", "B", "C", "D", "E"] {
        h.add(&format!("{v}/V1.cbz"), 3);
    }
    // A: sealed, with its row; the canonical name is free -> renamed, row re-keyed.
    h.bunko_wrote("A/V1_1.mokuro", HAYAI, &primary());
    // B: a sealed layer orphan; the layer is there -> removed.
    std::fs::write(lib.join("B/V1.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("B/V1.fast.mokuro"), LEGACY).unwrap();
    std::fs::write(lib.join("B/V1.fast_2.mokuro"), sealed(HAYAI)).unwrap();
    // C: nothing proves it Bunko's -> left, counted.
    std::fs::write(lib.join("C/V1_1.mokuro"), HAYAI).unwrap();
    // D: a 0.7-era orphan (before the seal), its row the proof; the canonical name is
    // taken in another spelling -> removed, row forgotten.
    std::fs::write(lib.join("D/V1.mokuro.gz"), b"\x1f\x8b").unwrap();
    std::fs::write(lib.join("D/V1_2.mokuro"), LEGACY).unwrap();
    h.db.as_ref()
        .unwrap()
        .record_ocr_sidecar(&bunko_db::OcrSidecar {
            sidecar_path: "D/V1_2.mokuro".into(),
            volume_key: "D/V1.cbz".into(),
            generation_id: "g-1".into(),
            generation_name: "hayai-nova".into(),
            machine: "local".into(),
            engine: Some("hayai-nova".into()),
            ..Default::default()
        })
        .unwrap();
    // E: `V1_1` is a volume of its own: its sidecar is never an orphan.
    h.add("E/V1_1.cbz", 3);
    std::fs::write(lib.join("E/V1_1.mokuro"), sealed(HAYAI)).unwrap();

    h.at(0.0);

    let db = h.db.clone().unwrap();
    assert_eq!(listing(&lib.join("A")), ["V1.cbz", "V1.mokuro"]);
    assert_eq!(
        std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
        sealed(HAYAI),
        "the orphan's exact bytes"
    );
    assert!(db.get_ocr_sidecar("A/V1_1.mokuro").unwrap().is_none());
    let row = db
        .get_ocr_sidecar("A/V1.mokuro")
        .unwrap()
        .expect("re-keyed");
    assert_eq!(row.generation_id, "g-1");

    assert_eq!(
        listing(&lib.join("B")),
        ["V1.cbz", "V1.fast.mokuro", "V1.mokuro"]
    );
    assert_eq!(
        std::fs::read_to_string(lib.join("B/V1.fast.mokuro")).unwrap(),
        LEGACY,
        "the file at the name stays"
    );

    assert_eq!(listing(&lib.join("C")), ["V1.cbz", "V1_1.mokuro"]);

    assert_eq!(listing(&lib.join("D")), ["V1.cbz", "V1.mokuro.gz"]);
    assert!(db.get_ocr_sidecar("D/V1_2.mokuro").unwrap().is_none());

    assert_eq!(
        listing(&lib.join("E")),
        ["V1.cbz", "V1_1.cbz", "V1_1.mokuro"]
    );

    assert_eq!(h.s.orphans_unproven, 1);
    let raw = h.s.raw_status();
    assert_eq!(raw["orphans_unproven"], 1);
    let shaped =
        bunko_server::ocr::shape::shape_status(&raw, "normal", true, &mut Default::default());
    assert_eq!(shaped["orphans_unproven"], 1, "the admin sees the count");
    let visitor =
        bunko_server::ocr::shape::shape_status(&raw, "normal", false, &mut Default::default());
    assert!(
        visitor.get("orphans_unproven").is_none(),
        "a visitor does not"
    );

    // The next walk: only C's orphan is still there, and it is still counted.
    h.at(31.0);
    assert_eq!(h.s.owed.orphans, ["C/V1_1.mokuro"]);
    assert_eq!(h.s.orphans_unproven, 1);
    std::fs::remove_file(lib.join("C/V1_1.mokuro")).unwrap();
    h.at(62.0);
    assert!(h.s.owed.orphans.is_empty());
    assert_eq!(h.s.orphans_unproven, 0);
}
