//! A walk of a large cloud library takes many minutes of provider reads. What it has
//! judged joins the queue as it goes ([`bunko_server::ocr::owed::walk_in_parts`]), so
//! OCR starts on the first series instead of waiting for the whole library.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use bunko_server::ocr::owed::{KnownPages, OwedIndex, walk};
use bunko_server::ocr::sched::Msg;
use bunko_server::ocr::types::FileFacts;
use ocr_common::*;

/// The volumes of the library's first folder, as a running walk hands them over.
fn first_folder(h: &H) -> OwedIndex {
    let mut index = walk(
        &h.library(),
        &[primary()],
        &FileFacts,
        None,
        &KnownPages::new(),
    );
    index.volumes.retain(|rel, _| rel.starts_with("A/"));
    index
}

#[test]
fn a_running_walks_part_is_offered_at_once() {
    let mut h = harness(vec![primary()]);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    assert!(volumes(&p.drain()).is_empty(), "an empty library");
    // Volumes arrive; the next walk is running and has judged folder A.
    h.add("A/V1.cbz", 2);
    h.add("B/V1.cbz", 2);
    let part = first_folder(&h);
    let epoch = h.s.walk_epoch;
    h.run(|s| s.walking = true);
    h.s.handle(Msg::WalkedPart { epoch, part });
    assert_eq!(volumes(&p.drain()).len(), 1, "one volume is offered");
    let claimed: Vec<String> = h.s.claims.keys().map(|j| j.rel.to_string()).collect();
    assert_eq!(
        claimed,
        ["A/V1.cbz"],
        "folder A is offered before the walk ends"
    );
    assert!(h.s.walking, "the walk itself goes on");
}

#[test]
fn a_part_of_an_outdated_walk_is_dropped() {
    let mut h = harness(vec![primary()]);
    h.at(0.0);
    h.add("A/V1.cbz", 2);
    let part = first_folder(&h);
    // The settings changed during the walk: its parts are of the old rows.
    let stale = h.s.walk_epoch;
    h.run(|s| {
        s.walking = true;
        s.walk_epoch += 1;
    });
    h.s.handle(Msg::WalkedPart {
        epoch: stale,
        part: part.clone(),
    });
    assert!(h.s.owed.volumes.is_empty(), "{:?}", h.s.owed.volumes);
    // No walk runs: a late part is dropped too.
    let current = h.s.walk_epoch;
    h.run(|s| s.walking = false);
    h.s.handle(Msg::WalkedPart {
        epoch: current,
        part,
    });
    assert!(h.s.owed.volumes.is_empty());
}
