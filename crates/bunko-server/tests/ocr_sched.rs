//! The scheduler, driven deterministically: a manual clock, inline helper work, fake
//! processors on channels (spec ocr-scheduling §5–§16).

mod ocr_common;

use std::sync::Arc;

use bunko_proto::{Event, Op};
use bunko_server::ocr::sched::Msg;
use bunko_server::ocr::types::{Job, LibraryFacts};
use ocr_common::*;

fn names(v: &[(String, String, String)]) -> Vec<String> {
    v.iter()
        .map(|(_, _, a)| a.trim_start_matches("/mokuro-reader/").to_string())
        .collect()
}

#[test]
fn nothing_runs_without_a_processor() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    assert!(h.s.processing_hold().is_some());
    assert!(h.s.claims.is_empty());
    let hold = h.s.processing_hold().unwrap();
    assert_eq!(hold["reason"], "no-processor");
    assert_eq!(h.s.queue_hold(), Some("no-processor"));
}

#[test]
fn claim_order_round_robin_and_lookahead() {
    let mut h = harness(vec![primary()]);
    for rel in ["A/V1.cbz", "A/V2.cbz", "B/V1.cbz", "B/V2.cbz"] {
        h.add(rel, 4);
    }
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    assert_eq!(opened(&ops).len(), 1, "{ops:?}");
    // The session opens with the head of the queue and fills its 2-deep lookahead, series
    // taking turns.
    let v = volumes(&ops);
    assert_eq!(names(&v), vec!["A/V1.cbz", "B/V1.cbz"]);
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &v[0].1, 4));
    h.done(&p, &sid, &v[0].1, "V1.mokuro", 4, 2.0);
    // Installed beside the archive; the session topped up with the next turn.
    assert!(h.library().join("A/V1.mokuro").is_file());
    let v2 = volumes(&p.drain());
    assert_eq!(names(&v2), vec!["A/V2.cbz"]);
    h.event(&p, started(&sid, &v[1].1, 4));
    h.done(&p, &sid, &v[1].1, "V1.mokuro", 4, 2.0);
    assert!(h.library().join("B/V1.mokuro").is_file());
    assert_eq!(names(&volumes(&p.drain())), vec!["B/V2.cbz"]);
    assert_eq!(h.s.sessions[&sid].order.len(), 2);
}

#[test]
fn session_closes_when_the_queue_is_empty() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 2));
    h.done(&p, &sid, &claim, "V1.mokuro", 2, 1.0);
    let ops = p.drain();
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::CloseSession { sid: s } if *s == sid)),
        "{ops:?}"
    );
    h.event(&p, exit(&sid, Some(0)));
    assert!(h.s.sessions.is_empty());
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty());
}

#[test]
fn eft_leaves_a_volume_to_the_faster_lane() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 100);
    h.at(0.0);
    // Evidence: slow reads 1 page/s, fast 10 pages/s (steady volumes).
    for _ in 0..2 {
        h.s.rates().record_volume("g-1@slow", 100.0, 100.0, false);
        h.s.rates().record_volume("g-1@fast", 100.0, 10.0, false);
    }
    // Hold the queue while both connect, so neither claims before the other is a lane.
    h.run(|s| s.hold_queue("slow"));
    h.run(|s| s.hold_queue("fast"));
    let mut slow = h.connect("slow", 1);
    let mut fast = h.connect("fast", 1);
    // Both released at once: the slow lane (older) asks first.
    h.run(|s| {
        s.release_queue("slow");
        s.release_queue("fast");
    });
    assert!(
        volumes(&slow.drain()).is_empty(),
        "slow took the volume a faster lane finishes first"
    );
    let v = volumes(&fast.drain());
    assert_eq!(names(&v), vec!["A/V1.cbz"]);
    let claim = &h.s.claims[&Job::new("A/V1.cbz", "g-1")];
    assert_eq!(claim.pid, fast.pid);
}

#[test]
fn eft_lets_the_slow_lane_take_what_the_fast_lane_cannot_finish_sooner() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 100);
    h.add("B/V1.cbz", 100);
    h.at(0.0);
    for _ in 0..2 {
        h.s.rates().record_volume("g-1@slow", 100.0, 20.0, false);
        h.s.rates().record_volume("g-1@fast", 100.0, 10.0, false);
    }
    h.run(|s| s.hold_queue("slow"));
    h.run(|s| s.hold_queue("fast"));
    let mut slow = h.connect("slow", 1);
    let mut fast = h.connect("fast", 1);
    h.run(|s| {
        s.release_queue("slow");
        s.release_queue("fast");
    });
    // fast takes the head; the second volume would wait ~30 s behind it on fast but is
    // done in ~40 s on slow (both pay the 20 s default startup): within the margin, slow
    // keeps it.
    let f = names(&volumes(&fast.drain()));
    let s = names(&volumes(&slow.drain()));
    assert_eq!(f.len() + s.len(), 2, "fast {f:?} slow {s:?}");
    assert!(!s.is_empty(), "the slow lane took nothing: fast {f:?}");
}

#[test]
fn two_dead_sessions_stop_the_row_on_that_machine() {
    let mut h = harness(vec![primary()]);
    for rel in ["A/V1.cbz", "A/V2.cbz", "A/V3.cbz", "A/V4.cbz"] {
        h.add(rel, 2);
    }
    h.at(0.0);
    let mut p = h.connect("box", 1);
    for round in 0..2 {
        let ops = p.drain();
        let sid = opened(&ops)[0].clone();
        let claim = volumes(&ops)[0].1.clone();
        h.event(&p, ready(&sid));
        h.event(&p, started(&sid, &claim, 2));
        h.event(
            &p,
            Event::Fatal {
                sid: sid.clone(),
                error: "CUDA out of memory".into(),
            },
        );
        h.event(&p, exit(&sid, Some(1)));
        assert_eq!(
            h.s.failures.len(),
            round + 1,
            "the oldest delivered volume is blamed"
        );
    }
    assert!(
        h.s.stopped
            .contains(&("g-1".to_string(), "box".to_string()))
    );
    assert!(
        opened(&p.drain()).is_empty(),
        "a stopped row opens no session on that machine"
    );
    // Another machine still runs the row.
    let mut q = h.connect("other", 1);
    assert_eq!(opened(&q.drain()).len(), 1);
    assert!(
        opened(&p.drain()).is_empty(),
        "a connect between polls does not forgive the row"
    );
    let rec = h.s.failures.values().next().unwrap();
    assert!(
        rec["error"]
            .as_str()
            .unwrap()
            .contains("CUDA out of memory"),
        "{rec}"
    );
}

#[test]
fn a_runner_that_never_starts_backs_off_and_blames_nothing() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    h.event(
        &p,
        Event::Fatal {
            sid: sid.clone(),
            error: "model file missing".into(),
        },
    );
    h.event(&p, exit(&sid, Some(1)));
    assert!(
        h.s.failures.is_empty(),
        "an environment failure blames no volume"
    );
    assert!(h.s.claims.is_empty());
    let backoff =
        h.s.start_backoff
            .get(&("g-1".to_string(), "box".to_string()))
            .expect("backoff recorded");
    assert_eq!(backoff.failures, 1);
    assert!(
        opened(&p.drain()).is_empty(),
        "backed off: no new session yet"
    );
    // The wait (poll interval × 4^0 = 30 s) runs out: the next scan tries again.
    h.at(31.0);
    assert_eq!(opened(&p.drain()).len(), 1);
}

#[test]
fn disconnect_returns_claims_unrecorded() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 2));
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    assert!(h.s.failures.is_empty());
    assert!(h.s.sessions.is_empty());
    assert!(
        !h.s.attempted.contains(&Job::new("A/V1.cbz", "g-1")),
        "offered again this scan"
    );
    // A late volume_done from the dropped processor is ignored.
    h.done(&p, &sid, &claim, "V1.mokuro", 2, 1.0);
    assert!(!h.library().join("A/V1.mokuro").exists());
    // Another processor takes it in the same scan.
    let mut q = h.connect("other", 1);
    assert_eq!(names(&volumes(&q.drain())), vec!["A/V1.cbz"]);
}

#[test]
fn returned_claims_open_the_breaker() {
    let mut h = harness(vec![primary()]);
    for i in 1..=5 {
        h.add(&format!("A/V{i}.cbz"), 2);
    }
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let mut returned = 0;
    let mut proven = false;
    for _ in 0..20 {
        let ops = p.drain();
        for (sid, claim, _) in volumes(&ops) {
            if !proven {
                // One delivered archive proves the path.
                proven = true;
                h.event(
                    &p,
                    Event::Fetch {
                        sid: sid.clone(),
                        id: claim.clone(),
                        state: "ready".into(),
                        detail: Default::default(),
                    },
                );
                h.event(&p, started(&sid, &claim, 2));
                continue;
            }
            h.event(
                &p,
                Event::VolumeReturned {
                    sid,
                    id: claim,
                    class: "rejected".into(),
                    error: "403 from a proxy".into(),
                    detail: Default::default(),
                },
            );
            returned += 1;
        }
        if h.s.breaker_open(&p.pid) {
            break;
        }
    }
    assert_eq!(returned, 3, "the third return in a row opens the breaker");
    assert!(h.s.breaker_open(&p.pid));
    assert!(
        h.s.failures.is_empty(),
        "returns are never failures of a volume"
    );
    assert!(
        h.s.every_machine_held(),
        "a held breaker holds that machine"
    );
    assert!(volumes(&p.drain()).is_empty());
}

#[test]
fn failure_backoff_waits_the_retry_delay() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.event(
        &p,
        Event::VolumeFailed {
            sid: sid.clone(),
            id: claim,
            error: "every page failed".into(),
        },
    );
    let rec =
        h.s.failures
            .get("A/V1.cbz")
            .expect("recorded under the bare key");
    assert_eq!(rec["attempts"], 1);
    h.event(&p, exit(&sid, Some(0)));
    assert!(p.drain().iter().all(|o| !matches!(o, Op::Volume(_))));
    h.at(29.0);
    assert!(volumes(&p.drain()).is_empty(), "still backing off at 29 s");
    h.at(31.0);
    assert_eq!(
        names(&volumes(&p.drain())),
        vec!["A/V1.cbz"],
        "retried after poll × 4^0 = 30 s"
    );
}

#[test]
fn settings_change_cancels_the_running_row() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    let mut changed = primary();
    changed.engine = "paddle-manga".into();
    let mut s = h.s.settings().clone();
    s.rows = vec![changed];
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
    let ops = p.drain();
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Cancel { sid: Some(s), .. } if *s == sid)),
        "{ops:?}"
    );
    assert!(h.s.failures.is_empty(), "a cancellation records nothing");
    assert!(!h.s.sessions.contains_key(&sid));
}

#[test]
fn pool_or_name_changes_never_cancel() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let sid = opened(&p.drain())[0].clone();
    let mut renamed = primary();
    renamed.name = "nova".into();
    let mut s = h.s.settings().clone();
    s.rows = vec![renamed];
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
    assert!(h.s.sessions.contains_key(&sid));
    assert!(!p.drain().iter().any(|o| matches!(o, Op::Cancel { .. })));
}

struct Short;
impl LibraryFacts for Short {
    fn missing_pages(&self, cbz: &std::path::Path) -> i64 {
        if cbz.ends_with("A/V1.cbz") { 5 } else { 0 }
    }
}

#[test]
fn missing_pages_withhold_layers() {
    let mut h = harness_with(vec![primary(), layer("g-2", "fast")], Arc::new(Short));
    h.add("A/V1.cbz", 2);
    h.add("A/V2.cbz", 2);
    std::fs::write(h.library().join("A/V1.mokuro"), "{}").unwrap();
    std::fs::write(h.library().join("A/V2.mokuro"), "{}").unwrap();
    h.at(0.0);
    let v1 = &h.s.owed.volumes["A/V1.cbz"];
    assert!(v1.rows.is_empty(), "no layer for a volume short of pages");
    assert_eq!(v1.skipped.len(), 1);
    assert_eq!(
        h.s.owed.volumes["A/V2.cbz"].rows.len(),
        1,
        "a whole volume still gets its layer"
    );
    let skipped = h.s.skipped_missing_pages();
    assert_eq!(skipped[0]["volume"], "V1");
    assert_eq!(skipped[0]["missing_pages"], 5);
    assert_eq!(skipped[0]["generations"][0], "fast");
}

#[test]
fn collection_discards_a_result_whose_archive_changed() {
    let mut h = harness(vec![primary()]);
    let cbz = h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 2));
    // The archive is replaced while it is read.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_cbz(&cbz, 3);
    h.done(&p, &sid, &claim, "V1.mokuro", 2, 1.0);
    assert!(
        !h.library().join("A/V1.mokuro").exists(),
        "a stale result is never installed"
    );
    assert!(h.s.failures.is_empty(), "and not a failure of the volume");
    // Released for a fresh claim of the new file, at once.
    let again = volumes(&p.drain());
    assert_eq!(again.len(), 1, "re-offered this scan");
    assert_ne!(again[0].1, claim);
}

#[test]
fn a_layer_is_stamped_and_named_after_its_row() {
    let mut h = harness(vec![primary(), layer("g-2", "fast")]);
    h.add("A/V1.cbz", 2);
    std::fs::write(
        h.library().join("A/V1.mokuro"),
        r#"{"version":"0.2.5","volume_uuid":"u-1","pages":[]}"#,
    )
    .unwrap();
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let (_, claim, _) = volumes(&ops)[0].clone();
    let name = match ops.iter().find_map(|o| {
        if let Op::Volume(v) = o {
            Some(v.sidecar_name.clone())
        } else {
            None
        }
    }) {
        Some(n) => n,
        None => panic!("no volume op"),
    };
    assert_eq!(name, "V1.fast.mokuro");
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 2));
    h.done(&p, &sid, &claim, &name, 2, 1.0);
    let text = std::fs::read_to_string(h.library().join("A/V1.fast.mokuro")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["volume_uuid"], "u-1",
        "a layer inherits the primary's volume id"
    );
    assert_eq!(v["title"], "A");
    assert_eq!(v["ocr_engine"]["generation"], "fast");
}

#[test]
fn a_replaced_archive_cancels_its_session() {
    let mut h = harness(vec![primary()]);
    let cbz = h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let sid = opened(&p.drain())[0].clone();
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_cbz(&cbz, 5);
    h.s.handle(Msg::ArchiveArrived(cbz.clone()));
    let ops = p.drain();
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Cancel { sid: Some(s), .. } if *s == sid)),
        "{ops:?}"
    );
    assert!(h.s.failures.is_empty());
    // And it is queued again at once (the new file).
    assert!(
        !volumes(&p.drain()).is_empty()
            || !h.s.claims.is_empty()
            || h.s.owed.volumes.contains_key("A/V1.cbz")
    );
}

#[test]
fn bench_preempts_and_holds_its_machine() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.add("A/V2.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let sid = opened(&p.drain())[0].clone();
    assert!(!h.s.claims.is_empty());
    let req = bunko_server::ocr::sched::BenchRequest {
        key: "g-1".into(),
        processor: "box".into(),
        ..Default::default()
    };
    let v = h.s.bench_enqueue(req).expect("bench queued");
    assert_eq!(v["state"], "running");
    let ops = p.drain();
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Cancel { sid: Some(s), .. } if *s == sid)),
        "the machine's OCR was pre-empted: {ops:?}"
    );
    assert!(ops.iter().any(|o| matches!(o, Op::Bench(_))), "{ops:?}");
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty());
    assert_eq!(h.s.queue_hold(), Some("benchmarking"));
    let bid = ops
        .iter()
        .find_map(|o| {
            if let Op::Bench(b) = o {
                Some(b.bid.clone())
            } else {
                None
            }
        })
        .unwrap();
    let mut detail = std::collections::BTreeMap::new();
    detail.insert(
        "baseline".to_string(),
        serde_json::json!({"pages_per_second": 4.0}),
    );
    detail.insert(
        "best".to_string(),
        serde_json::json!({"pages_per_second": 5.0, "same_as_spec": true}),
    );
    h.event(
        &p,
        Event::BenchDone {
            bid: bid.clone(),
            detail,
        },
    );
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "done");
    assert!(
        h.s.queue_hold().is_none(),
        "the hold is released when the line empties"
    );
    assert!(
        !opened(&p.drain()).is_empty(),
        "OCR resumes on that machine"
    );
    // The machine's profile now carries its bench.
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let row = prof
        .row("box", "g-1", Some(&primary().output_affecting()))
        .unwrap();
    assert_eq!(row.bench_pages_per_second(), Some(5.0));
}

#[test]
fn autobench_runs_first_then_the_row_runs_untuned_when_it_fails() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    assert!(
        opened(&ops).is_empty(),
        "an unmeasured row is not offered: {ops:?}"
    );
    let bid = ops
        .iter()
        .find_map(|o| {
            if let Op::Bench(b) = o {
                Some(b.bid.clone())
            } else {
                None
            }
        })
        .expect("autobench sent");
    h.event(
        &p,
        Event::Fatal {
            sid: bid,
            error: "benchmarks are not implemented yet".into(),
        },
    );
    // Failed: the pair runs untuned from now on.
    let ops = p.drain();
    assert_eq!(opened(&ops).len(), 1, "{ops:?}");
}

/// Regression (review finding): a mismatched result name used to be joined under
/// `.processing/<sid>/<claim>/` and its PARENT removed — `../keep/x.mokuro` removed a
/// sibling directory (on Windows `C:x.mokuro` replaced the whole path). The cleanup now
/// removes `.processing/<sid>/<claim>` built from the server's ids only.
#[test]
fn a_mismatched_result_name_removes_only_its_claim_directory() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    let processing = h.storage().join(".processing");
    let claim_dir = processing.join(&sid).join(&claim);
    let keep = processing.join(&sid).join("keep");
    std::fs::create_dir_all(&claim_dir).unwrap();
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join("precious"), b"x").unwrap();
    std::fs::write(claim_dir.join("V1.mokuro"), b"{}").unwrap();
    for name in ["../keep/x.mokuro", "..", "C:x.mokuro", "/abs/x.mokuro"] {
        h.s.handle(Msg::ResultStored {
            pid: p.pid.clone(),
            sid: sid.clone(),
            claim: claim.clone(),
            name: name.into(),
            sha256: "0".repeat(64),
        });
        assert!(keep.join("precious").is_file(), "{name}");
    }
    assert!(!claim_dir.exists());
    // Ids that are not the server's own never reach the filesystem either.
    std::fs::create_dir_all(&claim_dir).unwrap();
    h.s.handle(Msg::ResultStored {
        pid: p.pid.clone(),
        sid: format!("{sid}/keep"),
        claim: "..".into(),
        name: "x.mokuro".into(),
        sha256: "0".repeat(64),
    });
    assert!(keep.join("precious").is_file());
    assert!(claim_dir.is_dir());
}
