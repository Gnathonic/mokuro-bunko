//! The queue file v2's view of the scheduler (`Scheduler::series_queue_snapshot`,
//! `docs/design/queue-file-v2.md` §5): every owed (volume, row) grouped by series, in
//! the state the plan gives it, and what the HTTP sink renders of it.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use bunko_proto::Event;
use bunko_sched::series_queue::{LiveJob, LiveState, Sink, Snapshot, Tuple};
use ocr_common::*;

fn jobs<'a>(s: &'a Snapshot, series: &str, stem: &str) -> &'a [LiveJob] {
    &s.series[series].vols[stem]
}

fn states(s: &Snapshot, series: &str, stem: &str) -> Vec<(String, LiveState)> {
    jobs(s, series, stem)
        .iter()
        .map(|j| (j.row.clone(), j.state))
        .collect()
}

#[test]
fn every_owed_job_by_series_with_the_rows_it_names() {
    let mut h = harness(vec![primary(), layer("l-1", "Lines")]);
    h.add("Series A/A 01.cbz", 3);
    h.add("Series A/A 02.cbz", 3);
    h.add("Series B/B 01.cbz", 3);
    h.add("Loose.cbz", 3);
    h.at(0.0);
    let s = h.s.series_queue_snapshot();
    assert_eq!(s.held.as_deref(), Some("no-processor"));
    assert_eq!(s.now, h.t0);
    let ids: Vec<&str> = s.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, [primary().name.as_str(), "Lines"]);
    assert_eq!(s.rows[0].kind, "ocr");
    assert_eq!(s.rows[1].kind, "layer");
    assert_eq!(s.rows[0].parts[0]["role"], "recognizer");
    // Keyed by the series' fold, spelled as the library spells it; a volume at the
    // library root has no series file.
    let keys: Vec<&str> = s.series.keys().map(String::as_str).collect();
    assert_eq!(keys, ["series a", "series b"]);
    assert_eq!(s.series["series a"].folder, "Series A");
    let stems: Vec<&str> = s.series["series a"]
        .vols
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(stems, ["A 01", "A 02"]);
    // Every job, the layer included (the v1 file listed only the next hundred).
    for stem in ["A 01", "A 02"] {
        assert_eq!(
            states(&s, "series a", stem),
            [
                (primary().name, LiveState::Queued { e: None }),
                ("Lines".to_string(), LiveState::Queued { e: None }),
            ]
        );
    }
    // A claim without a card (a local claim spooling a cloud archive): about to start.
    let row = primary();
    h.s.claims.insert(
        bunko_server::ocr::types::Job::new("Series B/B 01.cbz", &row.id),
        bunko_server::ocr::sched::Claimed {
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
    let s = h.s.series_queue_snapshot();
    assert_eq!(
        states(&s, "series b", "B 01"),
        [
            (primary().name, LiveState::Claimed),
            ("Lines".to_string(), LiveState::Queued { e: None }),
        ]
    );
}

#[test]
fn claimed_running_timed_and_backed_off_jobs() {
    let mut h = harness(vec![primary()]);
    h.add("A/A 1.cbz", 4);
    h.add("A/A 2.cbz", 4);
    h.add("B/B 1.cbz", 4);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let vols = volumes(&ops);
    assert_eq!(vols[0].2, "/mokuro-reader/A/A 1.cbz");
    let claim = vols[0].1.clone();
    // Sent to the processor: its card runs from now.
    let s = h.s.series_queue_snapshot();
    assert_eq!(s.held, None);
    let main = primary().name;
    let LiveState::Running { t0, .. } = jobs(&s, "a", "A 1")[0].state else {
        panic!("{:?}", states(&s, "a", "A 1"));
    };
    assert_eq!(t0, h.t0);
    h.event(&p, ready(&sid));
    h.at(5.0);
    h.event(&p, started(&sid, &claim, 4));
    let s = h.s.series_queue_snapshot();
    assert!(matches!(
        jobs(&s, "a", "A 1")[0].state,
        LiveState::Running { .. }
    ));
    // Done in 40 s: the rate is known, the next jobs are timed.
    h.at(45.0);
    h.done(&p, &sid, &claim, "A 1.mokuro", 4, 40.0);
    let s = h.s.series_queue_snapshot();
    assert!(!s.series["a"].vols.contains_key("A 1"), "no longer owed");
    let timed: Vec<f64> = ["A 2"]
        .iter()
        .map(|stem| jobs(&s, "a", stem)[0].state)
        .chain([jobs(&s, "b", "B 1")[0].state])
        .filter_map(|st| match st {
            LiveState::Queued { e } | LiveState::Running { e, .. } => e,
            LiveState::Claimed => None,
        })
        .collect();
    assert!(!timed.is_empty(), "{s:?}");
    assert!(timed.iter().all(|e| *e > s.now), "{timed:?}");
    // A failure puts its job in backoff: owed, outside the plan, untimed.
    let ops = p.drain();
    let next = volumes(&ops);
    assert!(!next.is_empty(), "the next volume goes out: {ops:?}");
    let (stem_series, stem) = if next[0].2.contains("/A/") {
        ("a", "A 2")
    } else {
        ("b", "B 1")
    };
    h.event(
        &p,
        Event::VolumeFailed {
            sid: sid.clone(),
            id: next[0].1.clone(),
            error: "CUDA out of memory".into(),
        },
    );
    let s = h.s.series_queue_snapshot();
    assert_eq!(
        states(&s, stem_series, stem),
        [(main, LiveState::Queued { e: None })]
    );
}

/// What the HTTP sink renders over a real run: a volume sent out shows `r` with its
/// start, and a finished job stays `r` until its series is settled, then leaves a
/// `done` entry.
#[test]
fn the_sink_follows_a_volume_through_its_run() {
    let mut h = harness(vec![primary()]);
    h.add("A/A 1.cbz", 4);
    h.at(0.0);
    let mut sink = Sink::new(0, None);
    let settled = |_: &str| true;
    let r = sink.refresh(&h.s.series_queue_snapshot(), &settled);
    assert_eq!(r.written.len(), 1);
    assert_eq!(sink.series["a"].vols["A 1"][0].1, Tuple::Queued(None));
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    // Sent to the processor: q → r is a write.
    let r = sink.refresh(&h.s.series_queue_snapshot(), &settled);
    assert_eq!(r.written.len(), 1, "{r:?}");
    let Tuple::Running { t0, .. } = sink.series["a"].vols["A 1"][0].1 else {
        panic!("{:?}", sink.series["a"]);
    };
    assert_eq!(t0, h.t0 as i64);
    // Its session starting, then the volume: nothing a reader needs.
    h.event(&p, ready(&sid));
    h.at(5.0);
    h.event(&p, started(&sid, &claim, 4));
    let r = sink.refresh(&h.s.series_queue_snapshot(), &settled);
    assert!(
        r.written
            .iter()
            .all(|(_, c)| *c != bunko_sched::series_queue::Class::Hard),
        "{r:?}"
    );
    h.at(45.0);
    h.done(&p, &sid, &claim, "A 1.mokuro", 4, 40.0);
    // The series' metadata is still to be recompiled: the job stays, running.
    let before = sink.series["a"].clone();
    let snap = h.s.series_queue_snapshot();
    let r = sink.refresh(&snap, &|_| false);
    assert!(r.written.is_empty(), "{r:?} {snap:?} {before:?}");
    assert!(
        matches!(sink.series["a"].vols["A 1"][0].1, Tuple::Running { t0: was, .. } if was == t0),
        "{:?}",
        sink.series["a"]
    );
    let r = sink.refresh(&h.s.series_queue_snapshot(), &settled);
    assert_eq!(r.written.len(), 1);
    assert!(sink.series["a"].vols.is_empty());
    assert_eq!(sink.series["a"].done["A 1"], (h.t0 + 45.0) as i64);
}

/// A volume queued, run and finished with no look of a sink in between (a fast
/// machine, a write window not yet come): the snapshot still names it finished, so its
/// series' file gets a `done` entry for it (regression: the rig's S4/S6 waited for an
/// OCR no series file ever announced).
#[test]
fn a_volume_finished_between_two_looks_is_still_named_done() {
    let mut h = harness(vec![primary()]);
    h.add("A/A 1.cbz", 4);
    h.at(0.0);
    let mut sink = Sink::new(0, None);
    let settled = |_: &str| true;
    // The sink's first look is before the volume arrives …
    let mut empty = h.s.series_queue_snapshot();
    empty.series.clear();
    assert!(sink.refresh(&empty, &settled).written.is_empty());
    // … and its next after it was finished.
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.at(5.0);
    h.event(&p, started(&sid, &claim, 4));
    h.at(25.0);
    h.done(&p, &sid, &claim, "A 1.mokuro", 4, 20.0);
    let snap = h.s.series_queue_snapshot();
    assert!(!snap.series.contains_key("a"), "nothing owed: {snap:?}");
    assert_eq!(snap.finished["a"].0, "A");
    assert_eq!(snap.finished["a"].1["A 1"], h.t0 + 25.0);
    // Not settled yet: nothing; settled: one write naming it done.
    assert!(sink.refresh(&snap, &|_| false).written.is_empty());
    let r = sink.refresh(&snap, &settled);
    assert_eq!(r.written.len(), 1, "{r:?}");
    let a = &sink.series["a"];
    assert!(a.vols.is_empty());
    assert_eq!(a.done["A 1"], (h.t0 + 25.0) as i64);
    assert!(
        sink.refresh(&h.s.series_queue_snapshot(), &settled)
            .written
            .is_empty()
    );
}
