//! Taking work by volume (`ocr.queue_order: volume`): the queue goes volume by volume,
//! one lane runs every job of a volume its machine can run, back to back, and other
//! machines are offered only what that machine cannot run (spec ocr-scheduling §4.2).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::collections::{BTreeMap, HashSet};

use bunko_core::generations::Generation;
use bunko_proto::{Event, Op, VolumeOp};
use bunko_server::ocr::sched::{Msg, OWNER_PATIENCE_SECONDS, VolumeOwner};
use ocr_common::*;
use serde_json::{Value, json};

/// A main `.mokuro` from before Bunko: the primary is done, nothing to upgrade.
const MAIN: &str = r#"{"version": "0.2.1", "title": "C", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;

fn by_volume(rows: Vec<Generation>) -> H {
    let mut h = harness(rows);
    h.s.settings.by_volume = true;
    h
}

fn lines() -> Generation {
    layer("g-2", "Lines")
}

/// A layer another engine runs (one the `hayai_only` machines do not have).
fn paddle() -> Generation {
    let mut g = layer("g-2", "Paddle");
    g.engine = "paddle-manga".into();
    g
}

fn order(h: &mut H) -> Vec<(String, String)> {
    h.s.candidates(&HashSet::new(), false)
        .iter()
        .map(|j| (j.rel.to_string(), j.gid.to_string()))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(r, g)| (r.to_string(), g.to_string()))
        .collect()
}

fn volume_ops(ops: &[Op]) -> Vec<VolumeOp> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Volume(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

fn archives(ops: &[Op]) -> Vec<String> {
    volume_ops(ops)
        .iter()
        .map(|v| v.archive.trim_start_matches("/mokuro-reader/").to_string())
        .collect()
}

/// The generation of the session each volume op of `ops` was sent to.
fn rows_of(h: &H, ops: &[Op]) -> Vec<String> {
    volume_ops(ops)
        .iter()
        .map(|v| h.s.sessions[&v.sid].row.id.clone())
        .collect()
}

fn owner(h: &H, rel: &str) -> Option<String> {
    h.s.volume_owners
        .get(rel)
        .and_then(|o| o.first())
        .map(|o| o.pid.clone())
}

/// Every machine holding `rel`'s volume, the owner first.
fn holders(h: &H, rel: &str) -> Vec<String> {
    h.s.volume_owners
        .get(rel)
        .into_iter()
        .flatten()
        .map(|o| o.pid.clone())
        .collect()
}

/// A second layer the `paddle` engine runs.
fn paddle_2() -> Generation {
    let mut g = layer("g-3", "Paddle 2");
    g.engine = "paddle-manga".into();
    g
}

/// Run the one claim of session `sid` to its end and let the session exit.
fn finish(h: &mut H, p: &Proc, op: &VolumeOp) {
    h.event(p, ready(&op.sid));
    h.event(p, started(&op.sid, &op.claim, 3));
    h.done(p, &op.sid, &op.claim, &op.sidecar_name, 3, 1.0);
    h.event(p, exit(&op.sid, Some(0)));
}

#[test]
fn volumes_come_in_the_order_of_their_first_job() {
    let mut h = by_volume(vec![primary(), lines()]);
    for rel in ["A/V1.cbz", "A/V2.cbz", "B/V1.cbz", "C/V1.cbz"] {
        h.add(rel, 3);
    }
    std::fs::write(h.library().join("C/V1.mokuro"), MAIN).unwrap();
    h.at(0.0);
    assert_eq!(
        order(&mut h),
        pairs(&[
            ("A/V1.cbz", "g-1"),
            ("A/V1.cbz", "g-2"),
            ("B/V1.cbz", "g-1"),
            ("B/V1.cbz", "g-2"),
            ("A/V2.cbz", "g-1"),
            ("A/V2.cbz", "g-2"),
            ("C/V1.cbz", "g-2"),
        ])
    );
    h.s.settings.by_volume = false;
    assert_eq!(
        order(&mut h),
        pairs(&[
            ("A/V1.cbz", "g-1"),
            ("B/V1.cbz", "g-1"),
            ("A/V2.cbz", "g-1"),
            ("A/V1.cbz", "g-2"),
            ("B/V1.cbz", "g-2"),
            ("C/V1.cbz", "g-2"),
            ("A/V2.cbz", "g-2"),
        ]),
        "by generation"
    );
}

/// The queue file follows the order: by volume the first 100 waiting jobs are 50
/// volumes' both jobs, not 100 volumes' main files.
#[test]
fn the_queue_file_lists_volumes_with_all_their_jobs() {
    let mut h = by_volume(vec![primary(), lines()]);
    for i in 1..=60 {
        h.add(&format!("A/V{i:03}.cbz"), 3);
    }
    h.at(0.0);
    let (_, vols, pending) = h.s.queue_document();
    assert_eq!(pending, 60);
    assert_eq!(vols.len(), 50);
    for v in &vols {
        let kinds: Vec<&str> = v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["ocr", "layer"], "{}", v["volume"]);
    }
    assert_eq!(vols[0]["volume"], "V001");
    assert_eq!(vols[49]["volume"], "V050");
}

#[test]
fn a_lane_runs_every_job_of_its_volume_before_the_next() {
    let mut h = by_volume(vec![primary(), lines()]);
    let a = h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    assert_eq!(opened(&ops).len(), 1, "{ops:?}");
    assert_eq!(
        archives(&ops),
        ["A/V1.cbz"],
        "no lookahead into B/V1: the lane's next job is A/V1's layer"
    );
    let first = volume_ops(&ops).remove(0);
    assert!(first.keep_archive, "more of A/V1 follows on this machine");
    assert_eq!(
        first.kept_archives,
        Some(vec!["/mokuro-reader/A/V1.cbz".to_string()]),
        "the archives this machine still runs: a copy of any other goes"
    );
    let meta = bunko_store::fs::metadata(&a).unwrap();
    assert_eq!(
        first.etag.as_deref(),
        Some(bunko_dav::strong_file_etag(&meta).as_str())
    );
    assert_eq!(
        h.s.sessions[&first.sid].draining.as_deref(),
        Some("the lane's next job is another generation's")
    );
    assert_eq!(owner(&h, "A/V1.cbz"), Some(p.pid.clone()));

    finish(&mut h, &p, &first);
    let ops = p.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz"], "{ops:?}");
    assert_eq!(rows_of(&h, &ops), ["g-2"]);
    let second = volume_ops(&ops).remove(0);
    assert!(!second.keep_archive, "nothing of A/V1 follows it");
    assert_eq!(second.etag, first.etag);

    finish(&mut h, &p, &second);
    let ops = p.drain();
    assert_eq!(archives(&ops), ["B/V1.cbz"]);
    assert_eq!(rows_of(&h, &ops), ["g-1"]);
    assert_eq!(
        volume_ops(&ops)[0].kept_archives,
        Some(vec!["/mokuro-reader/B/V1.cbz".to_string()]),
        "A/V1 is done: its copy goes"
    );
    assert_eq!(owner(&h, "A/V1.cbz"), None, "A/V1 is done");
    assert_eq!(owner(&h, "B/V1.cbz"), Some(p.pid.clone()));
}

#[test]
fn another_machine_is_not_offered_a_volume_its_owner_can_run() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    let mut b = h.connect("b", 1);
    let ops = b.drain();
    assert_eq!(archives(&ops), ["B/V1.cbz"]);
    assert_eq!(
        rows_of(&h, &ops),
        ["g-1"],
        "B/V1's first job, not A/V1's layer"
    );
    assert_eq!(owner(&h, "A/V1.cbz"), Some(a.pid.clone()));
    assert_eq!(owner(&h, "B/V1.cbz"), Some(b.pid.clone()));
}

#[test]
fn a_job_the_owner_cannot_run_goes_to_another_machine() {
    let mut h = by_volume(vec![primary(), paddle()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect_with("a", 1, json!(["hayai-nova"]));
    let ops = a.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz"]);
    assert!(
        !volume_ops(&ops)[0].keep_archive,
        "a cannot run the rest of A/V1"
    );
    let mut b = h.connect("b", 1);
    let ops = b.drain();
    assert_eq!(
        archives(&ops),
        ["A/V1.cbz"],
        "while a still runs the main file"
    );
    assert_eq!(rows_of(&h, &ops), ["g-2"]);
    assert!(!volume_ops(&ops)[0].keep_archive);
    assert_eq!(
        owner(&h, "A/V1.cbz"),
        Some(a.pid.clone()),
        "a keeps the volume"
    );
}

/// Finding: the jobs the owner cannot run had no single taker. The machine that takes
/// the first of them holds the volume next: the rest of what the owner cannot run goes
/// to it alone, one after another from its kept copy, not to a third machine that
/// would download the archive again.
#[test]
fn the_jobs_the_owner_cannot_run_go_to_one_machine() {
    let mut h = by_volume(vec![primary(), paddle(), paddle_2()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect_with("a", 1, json!(["hayai-nova"]));
    assert_eq!(rows_of(&h, &a.drain()), ["g-1"]);
    let mut b = h.connect_with("b", 1, json!(["paddle-manga"]));
    let ops = b.drain();
    assert_eq!(rows_of(&h, &ops), ["g-2"]);
    let first = volume_ops(&ops).remove(0);
    assert!(first.keep_archive, "b runs g-3 next");
    assert_eq!(holders(&h, "A/V1.cbz"), [a.pid.clone(), b.pid.clone()]);
    let mut c = h.connect_with("c", 1, json!(["paddle-manga"]));
    assert!(
        archives(&c.drain()).is_empty(),
        "g-3 is b's: it keeps the archive"
    );

    finish(&mut h, &b, &first);
    let ops = b.drain();
    assert_eq!(rows_of(&h, &ops), ["g-3"]);
    assert!(
        !volume_ops(&ops)[0].keep_archive,
        "nothing more of A/V1 for b"
    );
    assert!(archives(&c.drain()).is_empty());
}

/// Finding: a holder with two lanes ran two jobs of the volume at once, so both
/// downloads overlapped and its kept copy served neither. Its jobs go to the one lane
/// that holds the volume.
#[test]
fn a_holder_runs_the_volume_on_one_lane() {
    let mut h = by_volume(vec![primary(), paddle(), paddle_2()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect_with("a", 1, json!(["hayai-nova"]));
    assert_eq!(rows_of(&h, &a.drain()), ["g-1"]);
    let mut b = h.connect_with("b", 2, json!(["paddle-manga"]));
    let ops = b.drain();
    assert_eq!(rows_of(&h, &ops), ["g-2"], "not g-2 and g-3 on two lanes");
    let first = volume_ops(&ops).remove(0);
    assert!(first.keep_archive);
    finish(&mut h, &b, &first);
    assert_eq!(rows_of(&h, &b.drain()), ["g-3"]);
}

/// When the owner lets the volume go, the next holder owns it.
#[test]
fn the_next_holder_owns_the_volume_once_the_owner_leaves() {
    let mut h = by_volume(vec![primary(), paddle(), paddle_2()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect_with("a", 1, json!(["hayai-nova"]));
    assert_eq!(rows_of(&h, &a.drain()), ["g-1"]);
    let mut b = h.connect_with("b", 1, json!(["paddle-manga"]));
    assert_eq!(rows_of(&h, &b.drain()), ["g-2"]);
    h.s.handle(Msg::Drop {
        pid: a.pid.clone(),
        reason: "test".into(),
    });
    assert_eq!(holders(&h, "A/V1.cbz"), [b.pid.clone()]);
    let mut c = h.connect_with("c", 1, json!(["paddle-manga"]));
    assert!(archives(&c.drain()).is_empty(), "g-3 is still b's");
}

/// Finding: switching to volume mode while claims were in flight split their volumes:
/// a claim taken by generation had no owner, so the volume's next job went to another
/// machine, which downloaded the archive again.
#[test]
fn switching_to_volume_order_keeps_volumes_in_flight_on_their_machine() {
    let mut h = harness(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let ops = a.drain();
    assert_eq!(archives(&ops)[0], "A/V1.cbz");
    assert_eq!(rows_of(&h, &ops)[0], "g-1");
    let mut s = h.s.settings().clone();
    s.by_volume = true;
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
    assert_eq!(owner(&h, "A/V1.cbz"), Some(a.pid.clone()));
    let mut b = h.connect("b", 1);
    assert!(
        archives(&b.drain()).is_empty(),
        "A/V1's layer is a's: a is fetching the archive"
    );
}

/// Finding: a machine without the first generation's engine must not start a volume at
/// its second job while a machine that can run the first is there (the volume would
/// be split, and its archive downloaded twice).
#[test]
fn a_volume_starts_at_its_first_job_while_some_machine_can_run_it() {
    let mut h = by_volume(vec![primary(), paddle()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    // `a` runs both rows but is held for now; `b` runs only the layer.
    h.run(|s| s.hold_queue("a"));
    let mut a = h.connect("a", 1);
    let mut b = h.connect_with("b", 1, json!(["paddle-manga"]));
    assert!(
        archives(&b.drain()).is_empty(),
        "A/V1 starts with its main file"
    );
    assert!(a.drain().iter().all(|o| !matches!(o, Op::Volume(_))));
    h.run(|s| s.release_queue("a"));
    let ops = a.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz"]);
    let first = volume_ops(&ops).remove(0);
    assert!(first.keep_archive);
    assert!(archives(&b.drain()).is_empty(), "a runs A/V1's layer too");
    finish(&mut h, &a, &first);
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    assert!(archives(&b.drain()).is_empty());
}

/// With nobody able to run a volume's first job, its later jobs are open: the machine
/// that takes one runs the rest of what it can.
#[test]
fn a_volume_whose_first_job_nobody_can_run_starts_at_the_next() {
    let mut h = by_volume(vec![primary(), paddle()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut b = h.connect_with("b", 1, json!(["paddle-manga"]));
    let ops = b.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz"]);
    assert_eq!(rows_of(&h, &ops), ["g-2"]);
    assert_eq!(owner(&h, "A/V1.cbz"), Some(b.pid.clone()));
    // A machine that can run the main file takes it: b cannot.
    let mut a = h.connect_with("a", 1, json!(["hayai-nova"]));
    let ops = a.drain();
    assert_eq!(rows_of(&h, &ops), ["g-1"]);
}

#[test]
fn the_owners_disconnect_frees_its_volume() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    let mut b = h.connect("b", 1);
    assert!(archives(&b.drain()).is_empty(), "A/V1 is a's");
    h.s.handle(Msg::Drop {
        pid: a.pid.clone(),
        reason: "test".into(),
    });
    assert_eq!(owner(&h, "A/V1.cbz"), Some(b.pid.clone()));
    let ops = b.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz"]);
    assert_eq!(rows_of(&h, &ops), ["g-1"]);
}

#[test]
fn switching_to_generation_order_forgets_the_owners() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    assert!(h.s.volume_owners.contains_key("A/V1.cbz"));
    let mut s = h.s.settings().clone();
    s.by_volume = false;
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
    assert!(h.s.volume_owners.is_empty());
    assert!(h.s.volumes_started.is_empty());
    assert!(!h.s.claims.is_empty(), "the claim in flight goes on");
}

/// Finding: once its owner is gone, a half-done volume goes before volumes nobody
/// started, so its archive is fetched again soon (and likely still spooled), not
/// after every other volume's main file.
#[test]
fn a_started_volume_goes_before_new_ones_once_its_owner_leaves() {
    let mut h = by_volume(vec![primary(), lines()]);
    for rel in ["A/V1.cbz", "B/V1.cbz", "C/V1.cbz"] {
        h.add(rel, 3);
    }
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    assert_eq!(first.archive, "/mokuro-reader/A/V1.cbz");
    h.event(&a, ready(&first.sid));
    h.event(&a, started(&first.sid, &first.claim, 3));
    h.done(&a, &first.sid, &first.claim, &first.sidecar_name, 3, 1.0);
    h.s.handle(Msg::Drop {
        pid: a.pid.clone(),
        reason: "test".into(),
    });
    assert!(h.s.volume_owners.is_empty());
    assert_eq!(
        order(&mut h),
        pairs(&[
            ("A/V1.cbz", "g-2"),
            ("B/V1.cbz", "g-1"),
            ("B/V1.cbz", "g-2"),
            ("C/V1.cbz", "g-1"),
            ("C/V1.cbz", "g-2"),
        ])
    );
}

/// The patience valve: an owner that could run a job of its volume but does not take
/// one for [`OWNER_PATIENCE_SECONDS`] lets the volume go to any machine.
#[test]
fn an_owner_that_does_not_continue_gives_the_volume_up() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    // a's one lane is busy with A/V1, yet the books say it also runs B/V1 (a state
    // the claim never makes; the valve is for what nobody foresaw).
    let lane = h.s.claims.values().next().unwrap().lane;
    let now = h.s.mono();
    h.s.volume_owners.insert(
        "B/V1.cbz".into(),
        vec![VolumeOwner {
            pid: a.pid.clone(),
            lane,
            spool: None,
            stamp: None,
            since: now,
            more: true,
        }],
    );
    let mut b = h.connect("b", 1);
    assert!(
        archives(&b.drain()).is_empty(),
        "B/V1 is a's, A/V1's layer too"
    );
    h.s.volume_owners.get_mut("B/V1.cbz").unwrap()[0].since = now - OWNER_PATIENCE_SECONDS - 1.0;
    h.at(1.0);
    assert_eq!(archives(&b.drain()), ["B/V1.cbz"]);
    assert_eq!(owner(&h, "B/V1.cbz"), Some(b.pid.clone()));
}

/// Finding: a hold (a storage hold, an update, a benchmark) is no reason to give a
/// volume up: the owner keeps it however long the hold lasts.
#[test]
fn a_held_owner_keeps_its_volume() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    h.event(&a, ready(&first.sid));
    h.event(&a, started(&first.sid, &first.claim, 3));
    h.done(&a, &first.sid, &first.claim, &first.sidecar_name, 3, 1.0);
    h.run(|s| s.hold_queue("a"));
    h.event(&a, exit(&first.sid, Some(0)));
    assert!(h.s.claims.is_empty());
    let mut b = h.connect("b", 1);
    h.at(OWNER_PATIENCE_SECONDS * 3.0);
    assert!(archives(&b.drain()).is_empty(), "A/V1 waits for a");
    assert_eq!(owner(&h, "A/V1.cbz"), Some(a.pid.clone()));
    h.run(|s| s.release_queue("a"));
    assert_eq!(archives(&a.drain()), ["A/V1.cbz"]);
    assert!(archives(&b.drain()).is_empty());
}

#[test]
fn a_reused_archive_is_not_counted_as_a_download() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    let fetched = |h: &H| h.s.machines[&a.pid].transfer.to_value()["volumes"].clone();
    let detail = |reused: bool| -> BTreeMap<String, Value> {
        let mut d = BTreeMap::new();
        d.insert("bytes".into(), json!(1000));
        d.insert("seconds".into(), json!(if reused { 0.0 } else { 2.0 }));
        d.insert("requests".into(), json!(if reused { 0 } else { 1 }));
        if reused {
            d.insert("reused".into(), json!(true));
        }
        d
    };
    h.event(
        &a,
        Event::Fetch {
            sid: first.sid.clone(),
            id: first.claim.clone(),
            state: "ready".into(),
            detail: detail(true),
        },
    );
    assert_eq!(fetched(&h), 0);
    assert_eq!(
        h.s.cards[h.s.claims.keys().next().unwrap()]["delivered"],
        true
    );
    h.event(
        &a,
        Event::Fetch {
            sid: first.sid.clone(),
            id: first.claim.clone(),
            state: "ready".into(),
            detail: detail(false),
        },
    );
    assert_eq!(fetched(&h), 1);
}

/// By generation nothing changes: no lane keeps a volume, no archive is kept (the op
/// names none to keep), and the ETag a remote processor is told is the one a GET serves.
#[test]
fn by_generation_nothing_is_kept() {
    let mut h = harness(vec![primary(), lines()]);
    let a1 = h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    assert_eq!(archives(&ops), ["A/V1.cbz", "B/V1.cbz"]);
    let first = volume_ops(&ops).remove(0);
    assert!(!first.keep_archive);
    assert_eq!(first.kept_archives, Some(Vec::new()));
    let meta = bunko_store::fs::metadata(&a1).unwrap();
    assert_eq!(
        first.etag.as_deref(),
        Some(bunko_dav::strong_file_etag(&meta).as_str())
    );
    assert!(h.s.volume_owners.is_empty());
    assert!(h.s.volumes_started.is_empty());
}

/// Finding: after a switch to generation order a processor kept the copies it had kept
/// by volume for the rest of its connection: an op by generation named no archives
/// (`kept_archives` absent), which tells a processor nothing. Every remote op of this
/// library names them; by generation that is none, so the copies go.
#[test]
fn switching_to_generation_order_lets_kept_copies_go() {
    let mut h = by_volume(vec![primary(), lines()]);
    h.add("A/V1.cbz", 3);
    h.add("B/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    assert!(first.keep_archive);
    assert_eq!(
        first.kept_archives,
        Some(vec!["/mokuro-reader/A/V1.cbz".to_string()])
    );
    let mut s = h.s.settings().clone();
    s.by_volume = false;
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
    finish(&mut h, &a, &first);
    h.at(1.0);
    let next = volume_ops(&a.drain());
    assert!(!next.is_empty(), "a takes more work");
    for op in &next {
        assert!(!op.keep_archive);
        assert_eq!(op.kept_archives, Some(Vec::new()), "{}", op.archive);
    }
}

/// Finding: a copy a processor kept is the archive as it was when the op was sent (its
/// ETag matched the op's), but the claim took the archive's version when the processor
/// said it had it. An archive replaced outside WebDAV in between (no arrival cancels
/// the claim) then had the old version's result accepted for the new one. A reused copy
/// of an archive that changed since the op was sent sends the job back.
#[test]
fn a_reused_copy_of_an_archive_changed_since_the_op_is_sent_back() {
    let mut h = by_volume(vec![primary(), lines()]);
    let path = h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    let reused = || -> BTreeMap<String, Value> {
        let mut d = BTreeMap::new();
        d.insert("bytes".into(), json!(1000));
        d.insert("requests".into(), json!(0));
        d.insert("reused".into(), json!(true));
        d
    };
    // Replaced outside WebDAV: another size, so another stamp.
    ocr_common::write_cbz(&path, 4);
    h.event(
        &a,
        Event::Fetch {
            sid: first.sid.clone(),
            id: first.claim.clone(),
            state: "ready".into(),
            detail: reused(),
        },
    );
    assert!(
        !h.s.claims
            .values()
            .any(|c| c.claim.as_deref() == Some(first.claim.as_str())),
        "the claim on the old copy is released"
    );
    let ops = a.drain();
    assert!(
        ops.iter().any(|o| matches!(
            o,
            Op::Cancel { claim: Some(c), .. } if *c == first.claim
        )),
        "{ops:?}"
    );

    // The same version: the reused copy is taken, under the stamp the op was sent at.
    let mut h = by_volume(vec![primary(), lines()]);
    let path = h.add("A/V1.cbz", 3);
    let sent = bunko_server::ocr::types::stamp_of(&path);
    h.at(0.0);
    let mut a = h.connect("a", 1);
    let first = volume_ops(&a.drain()).remove(0);
    h.event(
        &a,
        Event::Fetch {
            sid: first.sid.clone(),
            id: first.claim.clone(),
            state: "ready".into(),
            detail: reused(),
        },
    );
    let claim =
        h.s.claims
            .values()
            .find(|c| c.claim.as_deref() == Some(first.claim.as_str()))
            .expect("still claimed");
    assert_eq!(claim.stamp, sent);
}
