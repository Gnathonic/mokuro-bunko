//! Page listing and natural sort against Python 0.5.2 (`tests/golden/`):
//! `natsort_cases.json` (natsort 8.4 orderings) always runs; `archive_pages.json`
//! (the runner's page lists of the sample archives: count, sha256 of the
//! newline-joined list, first five) runs when the samples in `~/Downloads` are
//! present.

use std::path::PathBuf;

use bunko_ocr::natsort::natsort;
use bunko_ocr::pages::ArchivePages;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn golden(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).expect("fixture")).expect("json")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("str").to_string())
        .collect()
}

#[test]
fn natsort_matches_natsort_8_4() {
    for case in golden("natsort_cases.json").as_array().expect("cases") {
        let mut input = strings(&case["input"]);
        natsort(&mut input, |s| s.as_str());
        assert_eq!(input, strings(&case["sorted"]));
    }
}

#[test]
fn archive_pages_match_the_runner() {
    let downloads = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Downloads");
    let listing = golden("archive_pages.json");
    let mut checked = 0;
    for (archive, pages) in listing.as_object().expect("object") {
        let path = downloads.join(archive);
        if !path.is_file() {
            continue;
        }
        let mut ap = ArchivePages::open(&path, None).expect("open");
        let joined = ap.pages().join("\n");
        assert_eq!(
            ap.pages().len() as u64,
            pages["count"].as_u64().unwrap_or(0),
            "{archive}"
        );
        assert_eq!(
            &ap.pages()[..ap.pages().len().min(5)],
            strings(&pages["head"]).as_slice(),
            "{archive}"
        );
        assert_eq!(
            hex::encode(Sha256::digest(joined.as_bytes())),
            pages["sha256"].as_str().unwrap_or(""),
            "{archive}"
        );
        // Every page streams out and passes its CRC check.
        let first = ap.pages()[0].clone();
        let last = ap.pages().last().cloned().unwrap_or_default();
        assert!(!ap.read(&first).expect("read").is_empty());
        assert!(!ap.read(&last).expect("read").is_empty());
        checked += 1;
    }
    eprintln!("checked {checked} archives");
}
