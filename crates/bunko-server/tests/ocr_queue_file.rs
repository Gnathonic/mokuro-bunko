//! The reader's queue file (`Scheduler::queue_document`): every running job, the jobs
//! about to start, then the next 100 waiting jobs that finish soonest, grouped by
//! volume; a volume carries only its listed jobs (spec ocr-scheduling §25).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::sync::Arc;

use bunko_core::config::UpgradeConfig;
use bunko_proto::Event;
use bunko_server::ocr::sched::Claimed;
use bunko_server::ocr::types::{FileFacts, Job};
use ocr_common::*;
use serde_json::Value;

/// A main `.mokuro` of three pages from before Bunko (no `ocr_engine`): an upgrade
/// candidate when upgrades are on, a finished main file otherwise.
const LEGACY: &str = r#"{"version": "0.2.1", "title": "A", "volume": "V1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;

fn enabled() -> UpgradeConfig {
    UpgradeConfig {
        enabled: true,
        replace: vec!["mokuro-legacy".into(), "mokuro".into()],
    }
}

fn name(i: usize) -> String {
    format!("V{i:03}")
}

/// A listed job: `(kind, id, state)`.
type Line = (String, String, String);

/// `(volume, [(kind, id, state)])` of every listed volume.
fn listed(vols: &[Value]) -> Vec<(String, Vec<Line>)> {
    vols.iter()
        .map(|v| {
            (
                v["volume"].as_str().unwrap().to_string(),
                v["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|j| {
                        (
                            j["kind"].as_str().unwrap().to_string(),
                            j["id"].as_str().unwrap().to_string(),
                            j["state"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn queue_file_lists_the_next_hundred_jobs_not_every_job_of_a_hundred_volumes() {
    let mut h = harness(vec![primary(), layer("l-1", "Lines")]);
    for i in 1..=101 {
        h.add(&format!("A/{}.cbz", name(i)), 4);
    }
    h.at(0.0);
    let (held, vols, pending) = h.s.queue_document();
    assert_eq!(held, Some("no-processor"));
    assert_eq!(pending, 101);
    assert_eq!(vols.len(), 100);
    let got = listed(&vols);
    for (i, (volume, jobs)) in got.iter().enumerate() {
        assert_eq!(*volume, name(i + 1));
        assert_eq!(jobs.len(), 1, "{volume}: {jobs:?}");
        assert_eq!(jobs[0].0, "ocr", "{volume}: {jobs:?}");
    }
    assert!(
        got.iter()
            .all(|(_, jobs)| jobs.iter().all(|j| j.0 != "layer")),
        "the layers are planned after every main file"
    );
}

#[test]
fn queue_file_leaves_out_the_later_layers_of_listed_volumes() {
    let mut h = harness(vec![
        primary(),
        layer("l-1", "Lines"),
        layer("l-2", "Words"),
    ]);
    for i in 1..=101 {
        let n = name(i);
        h.add(&format!("A/{n}.cbz"), 3);
        std::fs::write(h.library().join(format!("A/{n}.mokuro")), LEGACY).unwrap();
    }
    h.at(0.0);
    let (_, vols, pending) = h.s.queue_document();
    assert_eq!(pending, 101);
    assert_eq!(vols.len(), 100);
    for (volume, jobs) in listed(&vols) {
        let ids: Vec<(&str, &str)> = jobs.iter().map(|j| (&*j.0, &*j.1)).collect();
        assert_eq!(ids, [("layer", "Lines")], "{volume}");
    }
}

/// The owner's case: a volume listed for one job no longer brings along a job of its
/// planned long after. With upgrades on, each volume here owes an upgrade of its main
/// file (planned right after the main files nobody has) and a layer (planned after
/// every upgrade).
#[test]
fn queue_file_lists_upgrades_without_the_layers_planned_after_them() {
    let mut h = harness_full(
        vec![primary(), layer("l-1", "Lines")],
        Arc::new(FileFacts),
        Some(enabled()),
    );
    for i in 1..=101 {
        let n = name(i);
        h.add(&format!("A/{n}.cbz"), 3);
        std::fs::write(h.library().join(format!("A/{n}.mokuro")), LEGACY).unwrap();
    }
    h.at(0.0);
    assert!(h.s.owed.volumes["A/V001.cbz"].upgrade);
    let (_, vols, pending) = h.s.queue_document();
    assert_eq!(pending, 101);
    assert_eq!(vols.len(), 100);
    for (volume, jobs) in listed(&vols) {
        let kinds: Vec<&str> = jobs.iter().map(|j| &*j.0).collect();
        assert_eq!(kinds, ["ocr"], "{volume}");
    }
}

/// A job claimed whose run has not begun (a local claim on a cloud library spools the
/// archive first) is about to start: listed with the running ones, untimed, and not
/// counted among the 100 waiting jobs.
#[test]
fn queue_file_lists_a_claimed_job_as_queued() {
    let mut h = harness(vec![primary()]);
    for i in 1..=101 {
        h.add(&format!("A/{}.cbz", name(i)), 4);
    }
    h.at(0.0);
    let row = primary();
    let job = Job::new("A/V050.cbz", &row.id);
    h.s.claims.insert(
        job,
        Claimed {
            lane: 1,
            pid: "local".into(),
            machine: "this server".into(),
            sid: None,
            claim: None,
            stamp: None,
            settling: false,
            row,
            spool: None,
        },
    );
    let (_, vols, pending) = h.s.queue_document();
    let got = listed(&vols);
    assert_eq!(got[0].0, "V050");
    assert_eq!(
        got[0].1,
        [("ocr".to_string(), primary().name, "queued".to_string())]
    );
    assert_eq!(vols[0]["jobs"][0]["eta"], Value::Null);
    assert_eq!(vols[0]["jobs"][0]["progress"], Value::Null);
    assert_eq!(vols.len(), 101, "the claimed one plus 100 waiting");
    assert!(got[1..].iter().all(|(v, _)| v != "V050"));
    assert_eq!(pending, 101, "a claimed job still waits");
}

/// A job in failure backoff is not planned, so the file leaves it out; the volume's
/// manifest still names it, untimed.
#[test]
fn queue_file_leaves_out_a_backed_off_job_the_manifest_keeps() {
    let mut h = harness(vec![primary(), layer("l-1", "Lines")]);
    h.add("A/V1.cbz", 2);
    h.add("B/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let vols = volumes(&ops);
    assert_eq!(vols[0].2, "/mokuro-reader/A/V1.cbz");
    h.event(&p, ready(&sid));
    h.event(
        &p,
        Event::VolumeFailed {
            sid: sid.clone(),
            id: vols[0].1.clone(),
            error: "CUDA out of memory".into(),
        },
    );
    assert!(h.s.failures.contains_key("A/V1.cbz"));
    let (_, docs, _) = h.s.queue_document();
    let a = docs
        .iter()
        .find(|d| d["series"] == "A")
        .expect("A/V1 is listed for its layer");
    let kinds: Vec<&str> = a["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["layer"], "{a}");
    let pending = h.s.volume_pending("A/V1.cbz", None);
    let main = pending
        .iter()
        .find(|e| e["kind"] == "ocr")
        .expect("the manifest still owes the main file");
    assert_eq!(main["eta"], Value::Null);
}

/// Finding: while one machine of several is held on its own (its benchmark), the plan
/// still priced waiting jobs on its idle lane at "now + run time". Those finishing
/// times move with the clock while the others stay put, so they crossed the queue
/// file's 300 s buckets in turn and the file reordered itself with nothing changing.
/// A held machine's lanes take no waiting job in the plan.
#[test]
fn a_machine_held_on_its_own_takes_no_waiting_job_in_the_plan() {
    let mut h = harness(vec![primary()]);
    for v in ["A/V1.cbz", "B/V1.cbz", "C/V1.cbz", "D/V1.cbz"] {
        h.add(v, 4);
    }
    h.at(0.0);
    for m in ["a", "b"] {
        for _ in 0..3 {
            h.s.rates()
                .record_volume(&bunko_sched::rate::rate_key("g-1", m), 4.0, 40.0, false);
        }
    }
    h.run(|s| s.hold_queue("b"));
    let mut a = h.connect("a", 1);
    let mut b = h.connect("b", 1);
    let _ = (a.drain(), b.drain());
    assert!(h.s.claims.values().all(|c| c.machine == "a"), "b is held");
    let eta = |e: &serde_json::Map<String, Value>| e["eta_at"].as_str().map(str::to_string);
    for t in [0.0, 600.0] {
        h.at(t);
        let running = h.s.running_jobs();
        let pending = h.s.pending_jobs();
        let items = bunko_server::ocr::sched::Scheduler::plan_items(&pending, &running);
        let plan = h.s.queue_plan(&running, &items, None);
        let busy_until = plan.running.iter().filter_map(eta).max().expect("a runs");
        assert!(!plan.pending.is_empty());
        for e in &plan.pending {
            let at = eta(e).expect("timed on a's lane");
            assert!(
                at > busy_until,
                "{} is planned on b's idle lane ({at} before a frees at {busy_until})",
                e["series"]
            );
        }
    }
    let (held, _, _) = h.s.queue_document();
    assert_eq!(held, None, "one machine held is no queue hold");
}
