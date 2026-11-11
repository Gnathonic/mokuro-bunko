//! The scheduler, driven deterministically: a manual clock, inline helper work, fake
//! processors on channels (spec ocr-scheduling §5–§16).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::sync::Arc;

use bunko_proto::{Event, Op};
use bunko_server::ocr::sched::Msg;
use bunko_server::ocr::types::{FileFacts, Job, LibraryFacts};
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

/// Regression (upgrade test): with `local_processing` on, the server logged "OCR is
/// waiting for hardware: local processing is off and no processor is connected" at
/// startup, about 30 ms before its own OCR came up. That wait is no wait.
#[test]
fn no_hold_line_while_this_servers_ocr_starts() {
    let mut h = harness(vec![primary()]);
    h.s.settings.local_processing = true;
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    assert!(h.s.processing_hold().is_some(), "held until it is up");
    assert_eq!(h.s.hold_notice(), None);
    assert!(!h.s.hold_logged, "nothing was logged");
    let _ops = local_up(&mut h, &["fp32"]);
    assert!(h.s.processing_hold().is_none());
    // It went away: now the wait is real, and says what it is.
    h.s.handle(Msg::Drop {
        pid: "local".into(),
        reason: "the local processor stopped".into(),
    });
    h.at(31.0);
    assert!(h.s.processing_hold().is_some());
    assert_eq!(
        h.s.hold_notice(),
        Some(
            "OCR is waiting for hardware: this server's OCR stopped and no processor is connected"
        )
    );
    assert!(h.s.hold_logged);
    // Off: the wait is the owner's choice, said once.
    let mut off = harness(vec![primary()]);
    off.add("A/V1.cbz", 3);
    off.at(0.0);
    assert_eq!(
        off.s.hold_notice(),
        Some("OCR is waiting for hardware: local processing is off and no processor is connected")
    );
    assert!(off.s.hold_logged);
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

/// Regression: a processor older than the release that taught the OCR a page format
/// (JPEG XL: 0.7.5) was handed volumes of those pages, failed every one with "no page
/// images found", and each failure held the volume back an hour for everyone. It is
/// offered only volumes it can read; the others wait, unrecorded, for one that can.
#[test]
fn a_processor_is_never_offered_pages_its_release_cannot_read() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 3);
    h.add_jxl("B/J1.cbz", 3);
    h.at(0.0);
    let mut old = h.connect_version("old", 2, "0.7.4");
    let got = names(&volumes(&old.drain()));
    assert_eq!(got, vec!["A/V1.cbz"], "0.7.4 cannot read JPEG XL pages");
    assert!(h.s.failures.is_empty());
    let mut new = h.connect_version("new", 1, "0.7.5");
    assert_eq!(names(&volumes(&new.drain())), vec!["B/J1.cbz"]);
}

/// Regression: the earliest-finish walk left a JPEG XL volume to a faster lane whose
/// release cannot read it, so the lane that could kept stepping aside.
#[test]
fn eft_never_leaves_a_volume_to_a_lane_that_cannot_read_it() {
    let mut h = harness(vec![primary()]);
    h.add_jxl("A/J1.cbz", 100);
    h.at(0.0);
    for _ in 0..2 {
        h.s.rates().record_volume("g-1@able", 100.0, 100.0, false);
        h.s.rates()
            .record_volume("g-1@fast-old", 100.0, 10.0, false);
    }
    h.run(|s| s.hold_queue("able"));
    h.run(|s| s.hold_queue("fast-old"));
    let mut able = h.connect_version("able", 1, "0.7.5");
    let mut fast_old = h.connect_version("fast-old", 1, "0.7.4");
    h.run(|s| {
        s.release_queue("able");
        s.release_queue("fast-old");
    });
    assert!(
        volumes(&fast_old.drain()).is_empty(),
        "0.7.4 cannot read it"
    );
    assert_eq!(
        names(&volumes(&able.drain())),
        vec!["A/J1.cbz"],
        "the lane that can read it stepped aside for one that cannot"
    );
}

/// Regression: a row's last volume was left to a warm session on a faster machine whose
/// release cannot read it.
#[test]
fn a_warm_session_that_cannot_read_the_last_volume_does_not_keep_it() {
    /// Page counts from the zip directory (what the metadata cache gives a server): the
    /// warm-session handoff needs one.
    struct ZipFacts;
    impl LibraryFacts for ZipFacts {
        fn missing_pages(&self, cbz: &std::path::Path) -> i64 {
            LibraryFacts::missing_pages(&bunko_server::ocr::types::FileFacts, cbz)
        }
        fn page_count(&self, cbz: &std::path::Path) -> Option<i64> {
            bunko_server::ocr::owed::archive_pages(cbz)
        }
    }
    let mut h = harness_with(vec![primary()], Arc::new(ZipFacts));
    h.add("A/V1.cbz", 100);
    h.add_jxl("B/J1.cbz", 100);
    h.at(0.0);
    for _ in 0..2 {
        h.s.rates().record_volume("g-1@able", 100.0, 100.0, false);
        h.s.rates()
            .record_volume("g-1@fast-old", 100.0, 10.0, false);
    }
    let mut fast_old = h.connect_version("fast-old", 1, "0.7.4");
    let ops = fast_old.drain();
    assert_eq!(names(&volumes(&ops)), vec!["A/V1.cbz"]);
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&fast_old, ready(&sid));
    h.event(&fast_old, started(&sid, &claim, 100));
    let mut able = h.connect_version("able", 1, "0.7.5");
    assert_eq!(
        names(&volumes(&able.drain())),
        vec!["B/J1.cbz"],
        "the last volume waited for a warm session that cannot read it"
    );
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

/// The catalog's "OCR failed" badge: a volume whose primary row has a live failure
/// record, with the public text of its kind (never the error, which names a
/// processor's paths). A replaced archive makes the record void: pending again.
#[test]
fn failing_volumes_carry_the_public_text_of_their_failure() {
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
            error: "no page images found in V1.cbz".into(),
        },
    );
    let failing = h.s.failing_volumes();
    assert_eq!(
        failing.get("a/v1.cbz").map(String::as_str),
        Some("This volume has no pages that could be decoded."),
        "{failing:?}"
    );
    // A new archive in its place: the record no longer speaks for it.
    let cbz = h.add("A/V1.cbz", 3);
    let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(h.t0 + 5.0);
    std::fs::File::options()
        .write(true)
        .open(&cbz)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(h.s.failing_volumes().is_empty());
}

/// A failure of the volume itself (nothing in it decodes) is parked: no backoff
/// brings it back to the release that failed it, however long it waits; a machine of a
/// release that has not tried it gets it at once.
#[test]
fn a_failure_of_the_volume_is_parked_until_another_release_can_try() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_version("box", 1, "0.7.6");
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let claim = volumes(&ops)[0].1.clone();
    h.event(&p, ready(&sid));
    h.event(
        &p,
        Event::VolumeFailed {
            sid: sid.clone(),
            id: claim,
            error: "no page images found in V1.cbz".into(),
        },
    );
    let rec = &h.s.failures["A/V1.cbz"];
    assert_eq!(rec["tried_by"], serde_json::json!(["0.7.6"]));
    assert_eq!(rec["library_version"], "0.7.0-test");
    h.event(&p, exit(&sid, Some(0)));
    h.at(365.0 * 86400.0);
    assert!(
        volumes(&p.drain()).is_empty(),
        "retried by the release that failed it"
    );
    let mut newer = h.connect_version("newer", 1, "0.7.7");
    assert_eq!(names(&volumes(&newer.drain())), vec!["A/V1.cbz"]);
}

/// An admin's "retry": the failure records go (one volume's, or all), so a parked
/// volume is offered again at once, even to the release that failed it.
#[test]
fn an_admin_retry_brings_a_parked_volume_back() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.add("B/V2.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_version("box", 2, "0.7.6");
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    h.event(&p, ready(&sid));
    for (_, claim, _) in volumes(&ops) {
        h.event(
            &p,
            Event::VolumeFailed {
                sid: sid.clone(),
                id: claim,
                error: "no page images found".into(),
            },
        );
    }
    h.event(&p, exit(&sid, Some(0)));
    assert_eq!(h.s.failures.len(), 2);
    h.at(86400.0);
    assert!(volumes(&p.drain()).is_empty());
    assert_eq!(h.s.retry_failed(Some("A/V1.cbz")), 1);
    assert!(h.s.failures.contains_key("B/V2.cbz"));
    h.at(86401.0);
    assert_eq!(names(&volumes(&p.drain())), vec!["A/V1.cbz"]);
    assert_eq!(h.s.retry_failed(None), 1);
    assert!(h.s.failures.is_empty());
}

/// A failure record stays while its archive is listed in any letter case, as on a
/// disk that ignores case: the series and volume in another case (and `.CBZ`) are the
/// one the library lists, the same on every host. Gone in every spelling, it goes.
#[test]
fn a_failure_record_follows_its_archive_in_any_case() {
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
            error: "no page images found".into(),
        },
    );
    h.event(&p, exit(&sid, Some(0)));
    assert!(h.s.failures.contains_key("A/V1.cbz"));
    let lib = h.library();
    std::fs::remove_dir_all(lib.join("A")).unwrap();
    write_cbz(&lib.join("a/v1.CBZ"), 2);
    h.s.prune_failures();
    assert!(h.s.failures.contains_key("A/V1.cbz"), "kept in any case");
    std::fs::remove_dir_all(lib.join("a")).unwrap();
    h.s.prune_failures();
    assert!(h.s.failures.is_empty(), "gone in every spelling");
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
            error: "CUDA out of memory".into(),
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

fn bench_op(ops: &[Op]) -> Option<bunko_proto::BenchOp> {
    ops.iter().find_map(|o| match o {
        Op::Bench(b) => Some(b.clone()),
        _ => None,
    })
}

fn detail(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

/// Enqueue a benchmark of `key` on `machine` and run what it caused (the sample is
/// packed inline): the op the processor got.
fn bench_on(
    h: &mut H,
    p: &mut Proc,
    key: &str,
    spec: Option<serde_json::Value>,
) -> bunko_proto::BenchOp {
    let req = bunko_server::ocr::sched::BenchRequest {
        key: key.into(),
        processor: p.name.clone(),
        spec,
        ..Default::default()
    };
    let v = h.s.bench_enqueue(req).expect("bench queued");
    assert_eq!(v["state"], "running");
    assert_eq!(v["waiting_for_queue"], false);
    h.at(0.0);
    bench_op(&p.drain()).expect("the bench op was sent")
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
    h.at(0.0);
    let ops = p.drain();
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Cancel { sid: Some(s), .. } if *s == sid)),
        "the machine's OCR was pre-empted: {ops:?}"
    );
    let op = bench_op(&ops).expect("bench sent");
    assert!(
        op.sample.ends_with(&format!("/bench/{}/sample", op.bid)),
        "{}",
        op.sample
    );
    assert_eq!(op.pages, 4, "two 2-page volumes: every page");
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", op.bid));
    assert!(sample.is_file(), "the sample waits for the processor");
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty());
    assert_eq!(h.s.queue_hold(), Some("benchmarking"));
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["sample"], serde_json::json!({"pages": 4, "volumes": 2}));
    assert_eq!(got["host"]["gpu"], "Test GPU");
    assert_eq!(got["preempted"][0]["volume"], "V1");
    h.event(
        &p,
        Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 4.0},
                "best": {"trial": 1, "pages_per_second": 5.0, "stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
            })),
        },
    );
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "done");
    assert!(!sample.exists(), "the sample is removed with the run");
    assert!(
        h.s.queue_hold().is_none(),
        "the hold is released when the line empties"
    );
    assert!(
        !opened(&p.drain()).is_empty(),
        "OCR resumes on that machine"
    );
    // The machine's profile now carries its bench (never `.ocr-bench.json`).
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let row = prof
        .row("box", "g-1", Some(&primary().output_affecting()))
        .unwrap();
    assert_eq!(row.bench_pages_per_second(), Some(5.0));
    assert!(!h.storage().join(".ocr-bench.json").exists());
}

#[test]
fn bench_done_is_completed_and_stored_as_0_5_2_did() {
    let mut row = primary();
    row.precision = "auto-balanced".into();
    row.pools.stage_workers.insert("detect".into(), 3);
    row.pools.stage_workers.insert("post".into(), 2);
    row.pools
        .stage_device
        .insert("engine".into(), "gpu:0".into());
    let mut h = harness(vec![row.clone()]);
    h.add("A/V1.cbz", 6);
    h.add("B/V1.cbz", 6);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32", "fp16", "bf16"]);
    p.drain();
    let op = bench_on(&mut h, &mut p, "g-1", None);
    // The measured row's own pools go out; the processor strips the widths.
    assert_eq!(op.spec.pools.stage_workers.get("detect"), Some(&3));
    assert_eq!(op.spec.precision, "auto-balanced");
    assert_eq!(op.spec.precision_pick, None);
    let bid = op.bid.clone();
    h.event(
        &p,
        Event::BenchReady {
            bid: bid.clone(),
            detail: detail(serde_json::json!({
                "startup_seconds": 9.952, "model_load_seconds": 7.1, "min_window_seconds": 20.0,
                "pages": 12, "tunable": true, "max_trials": 10,
                "stage_keys": ["detect", "engine", "post"],
                "stage_device": {"detect": "cpu", "engine": "gpu:0"},
            })),
        },
    );
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["progress"]["max_trials"], 10);
    assert_eq!(got["progress"]["trial"], 0);
    assert_eq!(got["host"]["devices"]["engine"], "gpu:0");
    assert_eq!(got["startup_seconds"], 9.952);
    h.event(
        &p,
        Event::BenchProgress {
            bid: bid.clone(),
            detail: detail(serde_json::json!({"trial": 1, "pass_index": 2, "pages_done": 7, "pages": 24,
                "stage_workers": {"detect": 3}, "pages_per_second": 4.9, "window_seconds": 3.1, "pages_measured": 6})),
        },
    );
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["progress"]["pass_index"], 2);
    assert_eq!(got["progress"]["max_trials"], 10);
    for (n, fmt, pps, accepted) in [(1, "bf16", 5.1157, true), (2, "fp32", 1.5431, false)] {
        h.event(
            &p,
            Event::BenchTrial {
                bid: bid.clone(),
                detail: detail(serde_json::json!({
                    "n": n, "note": format!("precision {fmt}"),
                    "stage_workers": {"detect": 3, "engine": 1, "post": 1},
                    "queue_capacity": {"detect": 4, "engine": 1, "post": 1},
                    "stage_device": {"detect": "cpu", "engine": "gpu:0"},
                    "seconds": 35.155, "pages_per_second": pps, "window_seconds": 27.758,
                    "pages_measured": 143, "passes": 5, "short_window": false,
                    "first_emission_at": 17.688, "last_emission_at": 45.445,
                    "accepted": accepted, "verdict": null, "bottleneck": "engine",
                    "stages": [], "queues": [], "precision": fmt,
                    "gpu_busy_pct": 80.8, "cpu_busy_pct": 54.5,
                })),
            },
        );
    }
    h.event(
        &p,
        Event::BenchDone {
            bid: bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 5.1157, "seconds_per_page": 0.1955, "window_seconds": 27.758},
                "best": {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {},
                         "pages_per_second": 5.1157, "seconds_per_page": 0.1955, "speedup": 1.0,
                         "window_seconds": 27.758, "precision": "bf16"},
                "precision": "bf16", "precision_mode": "auto-balanced",
                "precision_trials": [{"precision": "bf16", "pages_per_second": 5.1157, "chosen": true},
                                     {"precision": "fp32", "pages_per_second": 1.5431, "chosen": false}],
                "precision_why": "benchmark: bf16 5.12 p/s beat fp32 1.54 p/s",
                "peak_rss_mb": 4809, "peak_vram_mb": 994,
            })),
        },
    );
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "done", "{got:#}");
    let best = &got["best"];
    // The pins the search left alone, as they ran: detect ran at 3, post did not.
    assert_eq!(
        best["stage_workers"],
        serde_json::json!({"detect": 3, "post": "auto"})
    );
    assert_eq!(best["queue_capacity"], serde_json::json!({}));
    assert_eq!(best["stage_device"], serde_json::json!({"engine": "gpu:0"}));
    assert_eq!(best["same_as_spec"], false);
    assert!(
        best.get("precision").is_none(),
        "a precision is never a pool"
    );
    assert_eq!(
        best["gpu_busy_pct"], 80.8,
        "the winning trial's busy numbers"
    );
    assert_eq!(got["precision"], "bf16");
    assert_eq!(got["precision_mode"], "auto-balanced");
    assert_eq!(got["precision_trials"].as_array().unwrap().len(), 2);
    assert_eq!(got["peak_vram_mb"], 994);
    assert_eq!(got["progress"], serde_json::Value::Null);
    assert_eq!(got["estimates"]["volume_200_pages_seconds"], 39);
    assert_eq!(got["estimates"]["remaining_pages"], 12);
    assert_eq!(got["estimates"]["remaining_seconds"], 2);
    // The machine's profile: the 0.5.2 bench summary.
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let stored = prof
        .row("box", "g-1", Some(&row.output_affecting()))
        .unwrap();
    let bench = stored.bench.unwrap();
    let keys: Vec<&str> = bench.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "pages_per_second",
            "window_seconds",
            "gpu_busy_pct",
            "cpu_busy_pct",
            "startup_seconds",
            "host",
            "at",
            "precision",
            "precision_mode",
            "precision_trials",
            "precision_why"
        ]
    );
    assert_eq!(bench["host"]["devices"]["engine"], "gpu:0");
    assert_eq!(bench["at"], got["finished_at"]);
    // ... and the machine now runs the row at its pick.
    let spec = h.s.row_spec("box", &row);
    assert_eq!(spec.precision_pick.as_deref(), Some("bf16"));
    assert_eq!(
        spec.precision_why,
        "benchmark: bf16 5.12 p/s beat fp32 1.54 p/s"
    );
    // Where the precision stands on that machine, for every mode.
    let on = h.s.precision_on(&row);
    assert_eq!(on["box"]["auto-balanced"]["precision"], "bf16");
    assert_eq!(on["box"]["auto-balanced"]["bench"], "done");
    assert_eq!(on["box"]["auto-balanced"]["trials"][1]["precision"], "fp32");
    assert_eq!(
        on["box"]["auto-speed"]["bench"], "off",
        "autobench is off here"
    );
    assert_eq!(on["box"]["auto-accuracy"]["precision"], "bf16");
    assert!(on["box"]["auto-accuracy"].get("bench").is_none());
    assert_eq!(on["box"]["fp16"]["eligible"], true);
    // A mode change makes the stored bench stale: no pick any more.
    let mut speed = row.clone();
    speed.precision = "auto-speed".into();
    assert_eq!(h.s.row_spec("box", &speed).precision_pick, None);
}

#[test]
fn a_failed_benchmark_says_why_with_the_processors_words() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 4);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    p.drain();
    let op = bench_on(&mut h, &mut p, "g-1", None);
    h.event(
        &p,
        Event::Fatal {
            sid: op.bid.clone(),
            error: "could not load hayai-nova: no package".into(),
        },
    );
    assert_eq!(
        h.s.bench_get("g-1", "box")["state"],
        "running",
        "fatal waits for exit"
    );
    h.event(&p, exit(&op.bid, None));
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "failed");
    assert_eq!(
        got["error"],
        "the hayai-nova benchmark ended: could not load hayai-nova: no package"
    );
    // Without a fatal, with a status.
    let op = bench_on(&mut h, &mut p, "g-1", None);
    h.event(&p, exit(&op.bid, Some(1)));
    assert_eq!(
        h.s.bench_get("g-1", "box")["error"],
        "the hayai-nova benchmark ended with status 1 before it produced a result"
    );
}

#[test]
fn a_draft_is_measured_from_its_spec_and_kept_in_memory_only() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 4);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    p.drain();
    let spec = serde_json::json!({"engine": "ppocr-manga", "pools": {"stage_device": {}}});
    let op = bench_on(&mut h, &mut p, "draft-x1", Some(spec));
    assert_eq!(op.spec.engine, "ppocr-manga");
    h.event(
        &p,
        Event::BenchDone {
            bid: op.bid,
            detail: detail(serde_json::json!({"best": {"trial": 1, "pages_per_second": 9.0}})),
        },
    );
    let got = h.s.bench_get("draft-x1", "box");
    assert_eq!(got["state"], "done");
    assert_eq!(got["spec"]["engine"], "ppocr-manga");
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    assert!(
        prof.row("box", "draft-x1", None).is_none(),
        "a draft is never kept"
    );
}

#[test]
fn autobench_runs_first_then_the_row_runs_untuned_when_it_fails() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    h.at(0.0);
    let ops = p.drain();
    assert!(
        opened(&ops).is_empty(),
        "an unmeasured row is not offered: {ops:?}"
    );
    let op = bench_op(&ops).expect("autobench sent");
    assert!(!op.precision_only);
    assert!(h.s.bench_get("g-1", "box")["autobench"] == true);
    h.event(
        &p,
        Event::Fatal {
            sid: op.bid.clone(),
            error: "the models would not load".into(),
        },
    );
    h.event(&p, exit(&op.bid, None));
    // Failed: the pair runs untuned from now on.
    let ops = p.drain();
    assert_eq!(opened(&ops).len(), 1, "{ops:?}");
}

#[test]
fn autobench_applies_best_where_nobody_set_pools_and_then_offers_the_row() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    h.at(0.0);
    let op = bench_op(&p.drain()).expect("autobench sent");
    h.event(
        &p,
        Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 4.0},
                "best": {"trial": 2, "stage_workers": {"detect": 2}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 5.0},
                "precision": "fp32", "precision_mode": "auto-accuracy",
            })),
        },
    );
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let stored = prof
        .row("box", "g-1", Some(&primary().output_affecting()))
        .unwrap();
    assert_eq!(
        stored.pools["stage_workers"],
        serde_json::json!({"detect": 2})
    );
    let raw = prof.load("box");
    assert_eq!(raw["rows"]["g-1"]["pools_autobench"], serde_json::json!({}));
    // Measured: the row is offered there now, with the found pools.
    h.at(1.0);
    let ops = p.drain();
    let open = ops.iter().find_map(|o| match o {
        Op::OpenSession { generation, .. } => Some(generation.clone()),
        _ => None,
    });
    let open = open.expect("the row runs there now");
    assert_eq!(open.pools.stage_workers.get("detect"), Some(&2));
}

#[test]
fn hand_set_pools_get_a_precision_only_autobench_and_keep_their_pools() {
    let mut row = primary();
    row.precision = "auto-balanced".into();
    let mut h = harness(vec![row.clone()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    // A person set this machine's pools: no width tuning there, but a pick is owed.
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let pools = serde_json::json!({"stage_workers": {"detect": 1}, "queue_capacity": {}, "stage_device": {}});
    prof.set_pools(
        "gpubox",
        "g-1",
        pools.as_object().unwrap(),
        Some(&row.output_affecting()),
        false,
        false,
    );
    h.at(0.0);
    let mut p = h.connect_gpu("gpubox", &["fp32", "fp16", "bf16"]);
    h.at(0.0);
    let op = bench_op(&p.drain()).expect("a precision bench");
    assert!(op.precision_only);
    assert_eq!(
        op.spec.pools.stage_workers.get("detect"),
        Some(&1),
        "its pools as set"
    );
    h.event(
        &p,
        Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 5.0},
                "best": {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 5.0},
                "precision": "bf16", "precision_mode": "auto-balanced",
                "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0, "chosen": true},
                                     {"precision": "fp32", "pages_per_second": 2.0, "chosen": false}],
                "precision_why": "benchmark: bf16 5.00 p/s beat fp32 2.00 p/s",
            })),
        },
    );
    let stored = prof
        .row("gpubox", "g-1", Some(&row.output_affecting()))
        .unwrap();
    assert_eq!(
        stored.pools["stage_workers"],
        serde_json::json!({"detect": 1}),
        "never a pool from a precision-only run"
    );
    assert_eq!(stored.bench.unwrap()["precision"], "bf16");
    h.at(1.0);
    let open = p.drain().into_iter().find_map(|o| match o {
        Op::OpenSession { generation, .. } => Some(generation),
        _ => None,
    });
    let open = open.expect("the row runs there now");
    assert_eq!(open.precision_pick.as_deref(), Some("bf16"));
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

/// Regression (review finding): a processor that dropped off between uploading a result
/// and its `volume_done` left that result in `.processing/<sid>/<claim>` for good: the
/// drop forgot the upload, and nothing ever swept the directory. A session's end
/// removes its uploads; the drop now does too, and its claim goes back as before.
#[test]
fn a_processor_that_drops_off_leaves_no_uploaded_result_behind() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 3);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let (claim, sidecar) = ops
        .iter()
        .find_map(|o| match o {
            Op::Volume(v) => Some((v.claim.clone(), v.sidecar_name.clone())),
            _ => None,
        })
        .expect("a volume was sent");
    // Its result arrived, as the upload stores it; its `volume_done` never does.
    let session_dir = h.storage().join(".processing").join(&sid);
    let claim_dir = session_dir.join(&claim);
    std::fs::create_dir_all(&claim_dir).unwrap();
    std::fs::write(claim_dir.join(bunko_proto::RESULT_FILE), b"{}").unwrap();
    // A half-written upload of the same session.
    std::fs::create_dir_all(session_dir.join("v99")).unwrap();
    std::fs::write(
        session_dir
            .join("v99")
            .join(format!("{}.part", bunko_proto::RESULT_FILE)),
        b"{",
    )
    .unwrap();
    h.s.handle(Msg::ResultStored {
        pid: p.pid.clone(),
        sid: sid.clone(),
        claim: claim.clone(),
        name: sidecar,
        sha256: "0".repeat(64),
    });
    assert!(claim_dir.join(bunko_proto::RESULT_FILE).is_file());

    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });

    assert!(h.s.claims.is_empty(), "{:?}", h.s.claims);
    assert!(h.s.results.is_empty());
    assert!(!session_dir.exists(), "its uploads were left behind");
    assert!(h.s.failures.is_empty(), "a drop records nothing");
    assert!(!h.library().join("A/V1.mokuro").exists());
    // The volume goes to the next processor, whose upload has a directory of its own.
    let mut q = h.connect("other", 1);
    assert_eq!(names(&volumes(&q.drain())), ["A/V1.cbz"]);
}

/// This server's own hardware: a local machine on a channel, as `LocalUp` makes it.
fn local_up(h: &mut H, formats: &[&str]) -> tokio::sync::mpsc::UnboundedReceiver<Op> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let catalog: bunko_proto::Catalog = serde_json::from_value(serde_json::json!({
        "engines": ENGINES,
        "detectors": ["ppocr-manga"],
        "devices": [{"id": "cpu", "label": "CPU"},
                    {"id": "gpu:0", "label": "GPU 0", "formats": formats, "provider": "rocm", "arch": "gfx1201"}],
        "engine_revisions": current_revisions(ENGINES),
    }))
    .unwrap();
    let host = bunko_proto::HostInfo {
        cpu: "Test CPU".into(),
        gpu: Some("Test GPU".into()),
        backend: "rocm".into(),
        ..Default::default()
    };
    h.s.handle(Msg::LocalUp {
        ops: tx,
        catalog,
        host,
    });
    rx
}

fn drain_rx(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Op>) -> Vec<Op> {
    let mut out = Vec::new();
    while let Ok(op) = rx.try_recv() {
        out.push(op);
    }
    out
}

#[test]
fn this_servers_autobench_lands_in_its_own_profile_and_bench_file() {
    let mut row = primary();
    row.precision = "auto-speed".into();
    let mut h = harness(vec![row.clone()]);
    h.s.settings.autobench = true;
    h.s.settings.local_processing = true;
    h.add("A/V1.cbz", 6);
    h.at(0.0);
    let mut ops = local_up(&mut h, &["fp32", "fp16", "bf16"]);
    h.at(0.0);
    let sent = drain_rx(&mut ops);
    assert!(opened(&sent).is_empty(), "measured first: {sent:?}");
    let op = bench_op(&sent).expect("an autobench of this server");
    assert!(!op.precision_only);
    assert!(
        std::path::Path::new(&op.sample).is_file(),
        "the in-process processor reads the sample in place: {}",
        op.sample
    );
    let local = h.s.machines.values().find(|m| m.local).unwrap().pid.clone();
    let event = |h: &mut H, event: Event| {
        h.s.handle(Msg::Event {
            pid: local.clone(),
            event,
        })
    };
    event(
        &mut h,
        Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 8.0},
                "best": {"trial": 4, "stage_workers": {"detect": 8}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 10.0},
                "precision": "fp16", "precision_mode": "auto-speed",
                "precision_trials": [{"precision": "bf16", "pages_per_second": 8.1, "chosen": false},
                                     {"precision": "fp16", "pages_per_second": 8.6, "chosen": true},
                                     {"precision": "fp32", "pages_per_second": 1.9, "chosen": false}],
                "precision_why": "benchmark: fp16 8.60 p/s beat bf16 8.10 p/s, fp32 1.90 p/s",
            })),
        },
    );
    let got = h.s.bench_get("g-1", "local");
    assert_eq!(got["state"], "done");
    assert_eq!(
        got["host"]["cpu"]
            .as_str()
            .map(|c| c.starts_with("Test CPU")),
        Some(true)
    );
    // This server's result: `.ocr-bench.json` AND (an autobench) its own profile.
    let saved = bunko_sched::bench_file::BenchFile::new(&h.storage()).load();
    assert_eq!(saved["g-1"]["precision"], "fp16");
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let mine = prof
        .row(
            bunko_server::ocr::profiles::LOCAL_PROFILE,
            "g-1",
            Some(&row.output_affecting()),
        )
        .unwrap();
    assert_eq!(
        mine.pools["stage_workers"],
        serde_json::json!({"detect": 8})
    );
    assert_eq!(mine.bench.as_ref().unwrap()["precision"], "fp16");
    // ... and runs the row with them, at its pick.
    h.at(1.0);
    let sent = drain_rx(&mut ops);
    let open = sent.iter().find_map(|o| match o {
        Op::OpenSession { generation, .. } => Some(generation.clone()),
        _ => None,
    });
    let open = open.expect("the row runs here now");
    assert_eq!(open.precision_pick.as_deref(), Some("fp16"));
    assert_eq!(open.pools.stage_workers.get("detect"), Some(&8));
    // Its rate prior is the benchmark's until real runs come in.
    let bench = h.s.machine_bench("g-1", "local").unwrap();
    assert_eq!(bench.pages_per_second, Some(10.0));
}

#[test]
fn hand_set_pools_on_this_server_are_never_width_tuned() {
    let mut row = primary();
    row.pools.stage_workers.insert("detect".into(), 2);
    let mut h = harness(vec![row.clone()]);
    h.s.settings.autobench = true;
    h.s.settings.local_processing = true;
    h.add("A/V1.cbz", 6);
    h.at(0.0);
    let mut ops = local_up(&mut h, &["fp32", "fp16", "bf16"]);
    h.at(0.0);
    let sent = drain_rx(&mut ops);
    assert!(bench_op(&sent).is_none(), "{sent:?}");
    assert_eq!(opened(&sent).len(), 1, "the table is used as written");
}

/// Regression (a fresh desktop install): the owner benchmarked the row on this server by
/// hand, and the scheduler then benchmarked it again ("before it runs there") because a
/// benchmark by hand reached `.ocr-bench.json` but not this server's profile, the one
/// thing the autobench looks at. One benchmark per engine and machine: the volume runs
/// straight after it, untuned (a run by hand never changes the pools).
#[test]
fn a_benchmark_by_hand_on_this_server_is_not_run_again_before_its_first_volume() {
    let row = primary();
    let mut h = harness(vec![row.clone()]);
    h.s.settings.autobench = true;
    h.s.settings.local_processing = true;
    h.add("A/V1.cbz", 6);
    let mut ops = local_up(&mut h, &["fp32", "fp16", "bf16"]);
    let req = bunko_server::ocr::sched::BenchRequest {
        key: "g-1".into(),
        processor: "local".into(),
        ..Default::default()
    };
    let v = h.s.bench_enqueue(req).expect("bench queued");
    assert_eq!(v["autobench"], false);
    h.at(0.0);
    let sent = drain_rx(&mut ops);
    let op = bench_op(&sent).expect("the benchmark asked for");
    assert!(opened(&sent).is_empty(), "the benchmark holds the queue");
    let local = h.s.machines.values().find(|m| m.local).unwrap().pid.clone();
    h.s.handle(Msg::Event {
        pid: local,
        event: Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 8.0},
                "best": {"trial": 2, "stage_workers": {"detect": 5}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 8.9},
                "precision": "bf16", "precision_mode": "auto-accuracy",
            })),
        },
    });
    assert_eq!(h.s.bench_get("g-1", "local")["state"], "done");
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let mine = prof
        .row(
            bunko_server::ocr::profiles::LOCAL_PROFILE,
            "g-1",
            Some(&row.output_affecting()),
        )
        .expect("the run by hand is this server's benchmark of the row");
    assert_eq!(mine.bench.as_ref().unwrap()["pages_per_second"], 8.9);
    assert!(mine.pools.is_empty(), "a run by hand never sets pools");
    for t in [1.0, 2.0, 30.0] {
        h.at(t);
        let sent = drain_rx(&mut ops);
        assert!(
            bench_op(&sent).is_none(),
            "measured again at {t}s: {sent:?}"
        );
        if !opened(&sent).is_empty() {
            break;
        }
    }
    assert!(
        !h.s.claims.is_empty(),
        "the volume runs after the benchmark"
    );
    assert!(h.s.bench_get("g-1", "local")["autobench"] != true);
}

/// One autobench round on a device whose auto modes run fp32 (bf16 emulated or a CPU),
/// then the row runs there; the fp32 benchmark stays current through later scans.
fn fp32_device_autobenches_once(device: serde_json::Value) {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 4);
    h.add("A/V2.cbz", 4);
    h.at(0.0);
    let catalog = serde_json::json!({
        "engines": ENGINES,
        "detectors": ["ppocr-manga"],
        "devices": [{"id": "cpu", "label": "CPU", "formats": ["fp32", "bf16"], "provider": "cpu", "arch": "x86_64"}, device.clone()],
        "engine_revisions": current_revisions(ENGINES),
    });
    let mut p = h.connect_catalog("box", 1, catalog);
    h.at(0.0);
    let op = bench_op(&p.drain()).expect("one autobench");
    // auto-accuracy on this device is fp32: what the engines there will run.
    let row = primary();
    let on = h.s.precision_on(&row);
    assert_eq!(
        on["box"]["auto-accuracy"]["precision"], "fp32",
        "{device}: {on:#?}"
    );
    h.event(
        &p,
        Event::BenchDone {
            bid: op.bid.clone(),
            detail: detail(serde_json::json!({
                "baseline": {"pages_per_second": 2.7},
                "best": {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 2.7},
                "precision": "fp32", "precision_mode": "auto-accuracy",
            })),
        },
    );
    h.event(&p, exit(&op.bid, None));
    let prof = bunko_server::ocr::profiles::Profiles::new(&h.storage());
    let judged = h.s.machine_profile("box", &row).unwrap();
    assert!(!judged.stale_bench && judged.bench.is_some(), "{device}");
    assert!(
        prof.row("box", "g-1", Some(&row.output_affecting()))
            .unwrap()
            .bench
            .is_some()
    );
    // The volume runs there now, and later scans ask for no benchmark again.
    let mut benches = 0;
    let mut opened_any = false;
    for t in [1.0, 31.0, 62.0, 93.0] {
        h.at(t);
        let ops = p.drain();
        benches += ops.iter().filter(|o| matches!(o, Op::Bench(_))).count();
        opened_any |= !opened(&ops).is_empty();
        for sid in opened(&ops) {
            h.event(&p, ready(&sid));
        }
    }
    assert_eq!(benches, 0, "{device}: benchmarked again");
    assert!(opened_any, "{device}: the row never ran");
}

#[test]
fn rdna2_with_bf16_packages_runs_auto_modes_at_fp32_and_benchmarks_once() {
    fp32_device_autobenches_once(serde_json::json!(
        {"id": "gpu:0", "label": "GPU 0", "formats": ["fp32", "fp16", "bf16"], "provider": "rocm", "arch": "gfx1030"}
    ));
    fp32_device_autobenches_once(serde_json::json!(
        {"id": "gpu:0", "label": "GPU 0", "formats": ["fp32", "fp16", "bf16"], "provider": "rocm", "arch": "gfx1032"}
    ));
}

#[test]
fn turing_runs_auto_modes_at_fp32_and_benchmarks_once() {
    fp32_device_autobenches_once(serde_json::json!(
        {"id": "gpu:0", "label": "GPU 0", "formats": ["fp32", "fp16"], "provider": "cuda", "arch": "sm_75"}
    ));
}

#[test]
fn a_bf16_cpu_runs_auto_modes_at_fp32_and_benchmarks_once() {
    // A GPU with no package for it: the engines run the row on the CPU (AVX512_BF16,
    // a bf16 package), and the CPU's auto modes are fp32.
    fp32_device_autobenches_once(serde_json::json!(
        {"id": "gpu:0", "label": "no package", "formats": [], "provider": "cuda", "arch": "sm_89"}
    ));
}

#[test]
fn forced_bf16_still_runs_where_a_package_exists() {
    let mut row = primary();
    row.precision = "bf16".into();
    let h = {
        let mut h = harness(vec![row.clone()]);
        h.add("A/V1.cbz", 4);
        h.at(0.0);
        h
    };
    let mut h = h;
    let rdna2 = serde_json::json!({
        "engines": ["hayai-nova", "ppocr-manga"], "detectors": ["ppocr-manga"],
        "engine_revisions": current_revisions(["hayai-nova", "ppocr-manga"]),
        "devices": [{"id": "cpu", "label": "CPU", "provider": "cpu", "arch": "x86_64", "formats": ["fp32"]},
                    {"id": "gpu:0", "label": "GPU 0", "formats": ["fp32", "fp16", "bf16"], "provider": "rocm", "arch": "gfx1030"}],
    });
    let mut p = h.connect_catalog("rdna2", 1, rdna2);
    let on = h.s.precision_on(&row);
    assert_eq!(on["rdna2"]["bf16"]["eligible"], true);
    assert_eq!(on["rdna2"]["auto-accuracy"]["precision"], "fp32");
    h.at(1.0);
    assert_eq!(
        opened(&p.drain()).len(),
        1,
        "a forced bf16 row runs on RDNA2"
    );
    let turing = serde_json::json!({
        "engines": ["hayai-nova", "ppocr-manga"], "detectors": ["ppocr-manga"],
        "engine_revisions": current_revisions(["hayai-nova", "ppocr-manga"]),
        "devices": [{"id": "gpu:0", "label": "GPU 0", "formats": ["fp32", "fp16"], "provider": "cuda", "arch": "sm_75"}],
    });
    let _q = h.connect_catalog("turing", 1, turing);
    let on = h.s.precision_on(&row);
    assert_eq!(on["turing"]["bf16"]["eligible"], false, "{on:#?}");
    // A CPU with a bf16 package runs a forced bf16 row (the engines allow it there).
    let cpu = serde_json::json!({
        "engines": ["hayai-nova", "ppocr-manga"], "detectors": ["ppocr-manga"],
        "engine_revisions": current_revisions(["hayai-nova", "ppocr-manga"]),
        "devices": [{"id": "cpu", "label": "CPU", "formats": ["fp32", "bf16"], "provider": "cpu", "arch": "x86_64"},
                    {"id": "gpu:0", "label": "no package", "formats": [], "provider": "cuda", "arch": "sm_89"}],
    });
    let _c = h.connect_catalog("zen4", 1, cpu);
    let on = h.s.precision_on(&row);
    assert_eq!(on["zen4"]["bf16"]["eligible"], true, "{on:#?}");
    assert_eq!(on["zen4"]["auto-accuracy"]["precision"], "fp32");
}

/// A processor whose build of the row's engine is older than this library's revision
/// is offered none of its work: its output would be outdated the moment it landed, and
/// the upgrade would queue the volume again. One announcing the current revision is. A
/// 0.7 processor announces none, which counts as revision 1: refused like any other
/// older build once the engine is past its first revision.
#[test]
fn a_processor_with_an_older_engine_revision_gets_no_work() {
    let engine = primary().engine;
    let now = bunko_layout::sidecar::engine_revision(&engine);
    let older = now - 1;
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let catalog = |revision: u32| {
        serde_json::json!({
            "engines": [&engine],
            "detectors": ["ppocr-manga"],
            "devices": [{"id": "cpu", "label": "CPU"}],
            "engine_revisions": {&engine: revision},
        })
    };
    let mut stale = h.connect_catalog("stale", 1, catalog(older));
    assert!(
        opened(&stale.drain()).is_empty(),
        "offered to revision {older}"
    );
    let why = h.s.refusal(&stale.pid, &primary()).expect("a refusal");
    assert!(why.contains(&format!("revision {older},")), "{why}");
    let mut current = h.connect_catalog("current", 1, catalog(now));
    assert_eq!(opened(&current.drain()).len(), 1);
    assert!(h.s.refusal(&current.pid, &primary()).is_none());
    // A 0.7 processor announces no revisions: that is revision 1, whatever the engine
    // is at today. Refused when 1 is older than the library's, with those words.
    let old = h.connect_catalog(
        "old",
        1,
        serde_json::json!({
            "engines": [&engine],
            "detectors": ["ppocr-manga"],
            "devices": [{"id": "cpu", "label": "CPU"}],
        }),
    );
    let why = h.s.refusal(&old.pid, &primary());
    if now > 1 {
        let why = why.expect("a refusal of a 0.7 processor");
        assert!(why.contains("revision 1,"), "{why}");
    } else {
        assert!(why.is_none(), "{why:?}");
    }
}

/// An installed sidecar carries its install time, its revision (1 where the runner
/// wrote none) and a v1 content seal over the block that holds both.
#[test]
fn an_installed_sidecar_is_dated_revisioned_and_sealed() {
    let mut h = harness(vec![primary(), layer("g-2", "fast")]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let (_, claim, _) = volumes(&ops)[0].clone();
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, 2));
    h.done_with(
        &p,
        &sid,
        &claim,
        "V1.mokuro",
        2,
        1.0,
        br#"{"version":"0.2.5","title":"x","volume":"y","ocr_engine":{"id":"hayai-nova","generator":"mokuro-bunko"},"pages":[]}"#,
    );
    let bytes = std::fs::read(h.library().join("A/V1.mokuro")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let block = &v["ocr_engine"];
    assert_eq!(block["revision"], 1);
    let created = block["created"].as_str().expect("an install time");
    assert!(created.ends_with('Z') && created.len() >= 20, "{created}");
    assert!(block["seal"].as_str().unwrap().starts_with("v1:"));
    assert!(bunko_layout::seal::is_sealed(&bytes));
}

/// A processor's first job: `(processor, sid, claim, sidecar name)`, the volume started.
fn first_job(h: &mut H, pages: u32) -> (Proc, String, String, String) {
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
        .expect("a volume op");
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &claim, pages));
    (p, sid, claim, name)
}

/// The `ocr_sidecar_rejected` audit reasons.
fn rejected_reasons(h: &H) -> Vec<String> {
    h.db.as_ref()
        .unwrap()
        .list_audit_events(200, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "ocr_sidecar_rejected")
        .filter_map(|e| {
            let d: serde_json::Value = serde_json::from_str(e.details.as_deref()?).ok()?;
            d["reason"].as_str().map(str::to_string)
        })
        .collect()
}

/// Every file name in `dir`, sorted.
fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

const SUPERSEDED: &str = "a sidecar was already at the destination; the result was discarded";

/// A result in the shape a hayai-nova runner writes, at `revision`.
fn hayai_result(revision: u32) -> Vec<u8> {
    format!(
        r#"{{"version":"0.2.5","title":"x","volume":"y","ocr_engine":{{"id":"hayai-nova","revision":{revision},"detector":"ppocr-manga","generator":"mokuro-bunko"}},"pages":[]}}"#
    )
    .into_bytes()
}

/// The rename race (critique S6): a reader renamed a volume and PUT its main sidecar
/// while the OCR of the new name ran. The person's file stays, the result is
/// discarded (no `Vol_1.mokuro` beside it, ever), the job is not failed, and the next
/// walk owes nothing.
#[test]
fn a_main_sidecar_that_arrived_during_the_job_is_kept_and_the_result_discarded() {
    let mut h = harness_db(vec![primary()], Arc::new(FileFacts), None);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let (p, sid, claim, name) = first_job(&mut h, 2);
    let reader = r#"{"version":"0.2.5","title":"A","volume":"V1","pages":[]}"#;
    std::fs::write(h.library().join("A/V1.mokuro"), reader).unwrap();
    h.done_with(&p, &sid, &claim, &name, 2, 1.0, &hayai_result(1));
    assert_eq!(
        std::fs::read_to_string(h.library().join("A/V1.mokuro")).unwrap(),
        reader
    );
    assert_eq!(listing(&h.library().join("A")), ["V1.cbz", "V1.mokuro"]);
    assert_eq!(rejected_reasons(&h), [SUPERSEDED]);
    assert!(h.s.failures.is_empty(), "not a failure of the volume");
    assert!(h.s.claims.is_empty());
    h.at(31.0);
    assert!(h.s.claims.is_empty(), "owed nothing on the next walk");
    assert!(h.s.pending_jobs().is_empty());
}

/// A layer a reader rename moved in while the row ran (sealed, the same revision): kept
/// as it is, no `Vol.fast_1.mokuro`. A person's file at the row's name, Bunko's output a
/// person corrected and another engine's are kept even against a newer revision.
#[test]
fn a_layer_already_at_the_destination_is_kept() {
    let edited = {
        let mut v: serde_json::Value = serde_json::from_str(&sealed(
            r#"{"ocr_engine":{"id":"hayai-nova","revision":1},"pages":[]}"#,
        ))
        .unwrap();
        v["edited"] = serde_json::json!({"by": "mokuro-reader", "at": "2026-10-29T00:00:00Z"});
        v.to_string()
    };
    for (existing, result_revision) in [
        (
            sealed(
                r#"{"volume":"V1","ocr_engine":{"id":"hayai-nova","revision":1,"generation":"fast"},"pages":[]}"#,
            ),
            1,
        ),
        (
            r#"{"version":"0.2.5","pages":[{"img_path":"a.jpg","blocks":[]}]}"#.to_string(),
            2,
        ),
        (edited, 2),
        (
            sealed(r#"{"ocr_engine":{"id":"paddle-manga","revision":1},"pages":[]}"#),
            2,
        ),
        // Unsealed: only a stamp, which anyone can write.
        (
            r#"{"ocr_engine":{"id":"hayai-nova","revision":1},"pages":[]}"#.to_string(),
            2,
        ),
    ] {
        let mut h = harness_db(
            vec![primary(), layer("g-2", "fast")],
            Arc::new(FileFacts),
            None,
        );
        h.add("A/V1.cbz", 2);
        std::fs::write(
            h.library().join("A/V1.mokuro"),
            r#"{"version":"0.2.5","pages":[]}"#,
        )
        .unwrap();
        h.at(0.0);
        let (p, sid, claim, name) = first_job(&mut h, 2);
        assert_eq!(name, "V1.fast.mokuro");
        std::fs::write(h.library().join("A/V1.fast.mokuro"), &existing).unwrap();
        h.done_with(
            &p,
            &sid,
            &claim,
            &name,
            2,
            1.0,
            &hayai_result(result_revision),
        );
        assert_eq!(
            std::fs::read_to_string(h.library().join("A/V1.fast.mokuro")).unwrap(),
            existing
        );
        assert_eq!(
            listing(&h.library().join("A")),
            ["V1.cbz", "V1.fast.mokuro", "V1.mokuro"]
        );
        assert_eq!(rejected_reasons(&h), [SUPERSEDED]);
        assert!(h.s.failures.is_empty());
    }
}

/// Bunko's own sealed output of the same engine at a lower revision is replaced in
/// place by the newer result; nothing is kept beside it.
#[test]
fn bunkos_own_older_revision_at_the_destination_is_replaced_in_place() {
    let mut h = harness_db(
        vec![primary(), layer("g-2", "fast")],
        Arc::new(FileFacts),
        None,
    );
    h.add("A/V1.cbz", 2);
    std::fs::write(
        h.library().join("A/V1.mokuro"),
        r#"{"version":"0.2.5","pages":[]}"#,
    )
    .unwrap();
    h.at(0.0);
    let (p, sid, claim, name) = first_job(&mut h, 2);
    std::fs::write(
        h.library().join("A/V1.fast.mokuro"),
        sealed(r#"{"ocr_engine":{"id":"hayai-nova","revision":1,"generation":"fast"},"pages":[]}"#),
    )
    .unwrap();
    h.done_with(&p, &sid, &claim, &name, 2, 1.0, &hayai_result(2));
    let bytes = std::fs::read(h.library().join("A/V1.fast.mokuro")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ocr_engine"]["revision"], 2);
    assert!(bunko_layout::seal::is_sealed(&bytes));
    assert_eq!(
        listing(&h.library().join("A")),
        ["V1.cbz", "V1.fast.mokuro", "V1.mokuro"]
    );
    assert!(rejected_reasons(&h).is_empty());
}

// --- a large queue, and processors that come and go -------------------------------------

/// The ids of the benchmarks queued or running on `machine`, in line order.
fn line_bids(h: &H, machine: &str) -> Vec<String> {
    h.s.bench
        .lines
        .get(machine)
        .map(|l| l.iter().map(|r| r.bid.clone()).collect())
        .unwrap_or_default()
}

/// A processor that has measured none of its rows takes nothing until its benchmarks
/// land. Once a row's benchmark is asked for, its claims pass over that row's other
/// volumes without looking at them in the library. Each look lists the volume's folder
/// several times (a provider index read on a cloud library), and claims that looked at
/// every one of a large queue's volumes stalled the scheduler for seconds per claim.
/// What shows it here: volumes gone since the walk stay listed, as nothing looked.
#[test]
fn an_unmeasured_processor_passes_over_the_volumes_of_a_row_it_waits_to_measure() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    for v in 1..=4 {
        h.add(&format!("A/V{v}.cbz"), 2);
    }
    h.at(0.0);
    for v in 2..=4 {
        std::fs::remove_file(h.library().join(format!("A/V{v}.cbz"))).unwrap();
    }
    let mut p = h.connect_gpu("box", &["fp32"]);
    assert!(
        bench_op(&p.drain()).is_some(),
        "the row's benchmark is asked for"
    );
    for v in 2..=4 {
        assert!(
            h.s.owed.volumes.contains_key(&format!("A/V{v}.cbz")),
            "V{v} was looked at"
        );
    }
}

/// A processor reconnects whenever its link drops or its backend changes. One that
/// comes back unchanged carries on with the benchmarks it had queued: the one it was
/// running is sent again with the sample already packed, and none is asked for again.
/// Each ask packed a new sample from the library and logged "Benchmarking ...", for
/// every row on every reconnect.
#[test]
fn a_processor_back_unchanged_carries_on_with_its_benchmarks() {
    let mut h = harness(vec![primary(), layer("g-2", "fast")]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    let bids = line_bids(&h, "box");
    assert_eq!(bids.len(), 2, "one benchmark per row");
    assert_eq!(bids[0], first.bid);
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", first.bid));
    assert!(sample.is_file());
    for _ in 0..3 {
        h.s.handle(Msg::Drop {
            pid: p.pid.clone(),
            reason: "the socket closed".into(),
        });
        assert!(line_bids(&h, "box").is_empty(), "no line while it is away");
        assert!(!h.s.held("box"), "nothing is held for a machine away");
        p = h.connect_gpu("box", &["fp32"]);
        let again = bench_op(&p.drain()).expect("the running benchmark is sent again");
        assert_eq!(again.bid, first.bid);
        assert_eq!(line_bids(&h, "box"), bids, "no benchmark asked for again");
        assert!(sample.is_file(), "its sample is kept");
    }
    // It ends as any benchmark does, and the queued one follows.
    h.event(
        &p,
        Event::BenchDone {
            bid: first.bid.clone(),
            detail: detail(serde_json::json!({"best": {"trial": 1, "pages_per_second": 4.0}, "precision": "fp32", "precision_mode": "auto-accuracy"})),
        },
    );
    assert_eq!(h.s.bench_get("g-1", "box")["state"], "done");
    let next = bench_op(&p.drain()).expect("the queued benchmark follows");
    assert_eq!(next.bid, bids[1]);
}

/// A processor back with another catalog (a pack installed, a card gone) is measured
/// for what it is now: what it had queued is given up and asked for again.
#[test]
fn a_processor_back_changed_has_its_benchmarks_asked_again() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    let mut p = h.connect_gpu("box", &["fp32", "fp16"]);
    let again = bench_op(&p.drain()).expect("asked for again");
    assert_ne!(again.bid, first.bid);
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", first.bid));
    assert!(!sample.exists(), "the old sample is removed");
}

/// A processor that stays away has its benchmarks given up after
/// [`BENCH_PARK_SECONDS`], as one that left did at once before; back later, it is
/// measured afresh.
#[test]
fn a_processor_away_too_long_has_its_benchmarks_given_up() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    h.at(bunko_server::ocr::sched::BENCH_PARK_SECONDS + 60.0);
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "failed", "{got}");
    assert_eq!(got["error"], "box disconnected");
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", first.bid));
    assert!(!sample.exists(), "its sample is removed");
    let mut p = h.connect_gpu("box", &["fp32"]);
    let again = bench_op(&p.drain()).expect("measured afresh");
    assert_ne!(again.bid, first.bid);
}

/// A processor back while it installs its backend runs no benchmark (one would fail
/// there and leave the row unmeasured): its line carries on once it is available.
#[test]
fn a_processor_back_while_installing_carries_on_once_available() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    let catalog = h.s.machines[&p.pid].catalog_value.clone();
    let host = h.s.machines[&p.pid].host_value.clone();
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    // It registers again, unchanged but still installing (a pack it fetches).
    let mut p = reconnect_installing(&mut h, "box", host, catalog);
    assert!(
        bench_op(&p.drain()).is_none(),
        "nothing runs while it installs"
    );
    h.event(
        &p,
        Event::Availability(bunko_proto::Availability::default()),
    );
    let again = bench_op(&p.drain()).expect("its benchmark carries on");
    assert_eq!(again.bid, first.bid);
}

/// Register `name` again with `host` and `catalog`, paused while it installs, and open
/// its socket.
fn reconnect_installing(
    h: &mut H,
    name: &str,
    host: serde_json::Value,
    catalog: serde_json::Value,
) -> Proc {
    let username = format!("acct-{name}");
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    h.s.handle(Msg::Register {
        input: bunko_server::ocr::sched::RegisterInput {
            username: username.clone(),
            body: serde_json::json!({
                "protocol": 3, "name": name, "host": host, "catalog": catalog,
                "max_sessions": 1,
                "availability": {"paused": true, "reason": "installing"},
            }),
            account_stamp: None,
        },
        reply,
    });
    let pid = match rx.try_recv().unwrap() {
        bunko_server::ocr::sched::RegisterOutcome::Ok(r) => r.processor_id,
        other => panic!("register refused: {other:?}"),
    };
    let (tx, ops) = tokio::sync::mpsc::unbounded_channel();
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    h.s.handle(Msg::SocketOpen {
        pid: pid.clone(),
        username,
        ops: tx,
        reply,
    });
    rx.try_recv().unwrap().unwrap();
    Proc {
        pid,
        name: name.into(),
        ops,
    }
}

/// A benchmark parked for its machine's return shows as queued and can be cancelled.
/// It used to read as idle (or an old result) for up to ten minutes, and cancelling it
/// was refused.
#[test]
fn a_parked_benchmark_shows_as_queued_and_cancels() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "queued", "{got}");
    assert_eq!(got["waiting_for_machine"], true, "{got}");
    let got = h.s.bench_cancel("g-1", "box").expect("cancelled");
    assert_eq!(got["state"], "cancelled", "{got}");
    assert!(h.s.bench.parked.is_empty());
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", first.bid));
    assert!(!sample.exists(), "its sample is removed");
}

/// A clean stop ends the runs parked for processors that dropped off earlier and
/// removes their samples: no processor returns to this process, and nothing else
/// removes them from `.processing`.
#[test]
fn a_stop_removes_the_samples_of_parked_benchmarks() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    let first = bench_op(&p.drain()).expect("the first autobench");
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", first.bid));
    assert!(sample.is_file(), "kept for the return");
    h.s.handle(Msg::Stop);
    assert!(h.s.bench.parked.is_empty());
    assert!(!sample.exists(), "a parked sample outlived the stop");
}

/// A machine back but still paused when its parked runs expire did not disconnect: the
/// error says it never became available.
#[test]
fn a_parked_benchmark_of_a_paused_machine_expires_as_unavailable() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect_gpu("box", &["fp32"]);
    bench_op(&p.drain()).expect("the first autobench");
    let catalog = h.s.machines[&p.pid].catalog_value.clone();
    let host = h.s.machines[&p.pid].host_value.clone();
    h.s.handle(Msg::Drop {
        pid: p.pid.clone(),
        reason: "the socket closed".into(),
    });
    let _p = reconnect_installing(&mut h, "box", host, catalog);
    h.at(bunko_server::ocr::sched::BENCH_PARK_SECONDS + 60.0);
    let got = h.s.bench_get("g-1", "box");
    assert_eq!(got["state"], "failed", "{got}");
    assert_eq!(
        got["error"], "box did not become available within 10 minutes",
        "{got}"
    );
}

/// This server's own OCR turned on or off, as a settings save applies it (`Msg::Apply`).
fn set_local_processing(h: &mut H, on: bool) {
    let mut s = h.s.settings.clone();
    s.local_processing = on;
    h.s.handle(Msg::Apply {
        settings: Box::new(s),
        reply: None,
    });
}

/// Regression (owner report): this server's OCR, turned off in the settings mid-queue,
/// went on claiming volumes for itself for minutes, still weighed itself when placing
/// volumes and still showed as a machine. The setting retired only its idle lanes; the
/// busy one stayed, and its session never drained. Turned off, it stops at once: what it
/// ran goes back to the queue unrecorded and on to the processor that is connected,
/// nothing more is sent to it, it is no machine any more, nothing it ran is installed,
/// and its scratch directory goes once its processor has wound down. Turned on again,
/// it takes work as before.
#[test]
fn turning_local_processing_off_stops_this_servers_ocr_at_once() {
    let mut h = harness(vec![primary()]);
    h.s.settings.local_processing = true;
    for rel in ["A/V1.cbz", "A/V2.cbz"] {
        h.add(rel, 4);
    }
    h.at(0.0);
    let mut local = local_up(&mut h, &["fp32"]);
    h.at(1.0);
    let sent = drain_rx(&mut local);
    let sid = opened(&sent)[0].clone();
    let held = volumes(&sent);
    assert_eq!(held.len(), 2, "{sent:?}");
    let event = |h: &mut H, event: Event| {
        h.s.handle(Msg::Event {
            pid: "local".into(),
            event,
        })
    };
    event(&mut h, ready(&sid));
    event(&mut h, started(&sid, &held[0].1, 4));
    // The runner finished the other volume just as it was told to stop: its result
    // waits in the scratch directory, as the in-process processor leaves it.
    let scratch = h.storage().join(".processing").join(&sid);
    let result_dir = scratch.join(&held[1].1);
    std::fs::create_dir_all(&result_dir).unwrap();
    std::fs::write(result_dir.join(bunko_proto::RESULT_FILE), b"{}").unwrap();
    // A processor connects with nothing left to take.
    let mut p = h.connect("box", 1);
    assert!(volumes(&p.drain()).is_empty());

    set_local_processing(&mut h, false);

    assert!(!h.s.machines.contains_key("local"), "still a machine");
    assert!(
        h.s.lanes.iter().all(|l| l.pid != "local"),
        "{:?}",
        h.s.lanes
    );
    assert!(h.s.sessions.values().all(|s| !s.local));
    assert!(
        h.s.claims.values().all(|c| c.pid == p.pid),
        "{:?}",
        h.s.claims
    );
    assert!(h.s.failures.is_empty(), "turning it off records nothing");
    // What it held went to the processor that is connected.
    let mut taken = names(&volumes(&p.drain()));
    taken.sort();
    assert_eq!(taken, ["A/V1.cbz", "A/V2.cbz"]);
    // Nothing more goes to this server: its link is closed.
    for dt in [2.0, 40.0, 80.0] {
        h.at(dt);
    }
    let sent = drain_rx(&mut local);
    assert!(volumes(&sent).is_empty(), "{sent:?}");
    assert!(matches!(
        local.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(h.s.processors().iter().all(|m| m["local"] != true));
    assert!(
        h.s.connected_machines()
            .iter()
            .all(|m| m["machine"] != "local")
    );
    assert!(!h.library().join("A/V1.mokuro").exists());
    assert!(!h.library().join("A/V2.mokuro").exists());
    // Its scratch directory goes once its processor has wound down.
    assert!(
        scratch.is_dir(),
        "kept while its runner may still write there"
    );
    h.run(|s| s.sweep_local_scratch());
    assert!(!scratch.exists(), "left behind");

    // Turned on again: it comes up and takes work again.
    set_local_processing(&mut h, true);
    let mut local = local_up(&mut h, &["fp32"]);
    let arrived = h.add("B/V1.cbz", 4);
    h.s.handle(Msg::ArchiveArrived(arrived));
    h.at(81.0);
    let sent = drain_rx(&mut local);
    assert!(
        volumes(&sent)
            .iter()
            .any(|(_, _, a)| std::path::Path::new(a).ends_with("B/V1.cbz")),
        "{sent:?}"
    );
}

/// A lane of this server claims nothing while its own OCR is off, and its open session
/// drains, however the setting got there (defense in depth: turning it off drops this
/// server's machine, lanes and sessions at once).
#[test]
fn a_lane_of_this_server_claims_nothing_while_local_processing_is_off() {
    let mut h = harness(vec![primary()]);
    h.s.settings.local_processing = true;
    for rel in ["A/V1.cbz", "A/V2.cbz", "A/V3.cbz"] {
        h.add(rel, 4);
    }
    h.at(0.0);
    let mut local = local_up(&mut h, &["fp32"]);
    h.at(1.0);
    let sent = drain_rx(&mut local);
    let sid = opened(&sent)[0].clone();
    let held = volumes(&sent);
    assert_eq!(held.len(), 2, "{sent:?}");
    let lane = h.s.lanes.iter().find(|l| l.pid == "local").unwrap().id;
    // The setting flips under the lane, without the settings path.
    h.s.settings.local_processing = false;
    assert_eq!(h.s.claim(lane, Some("g-1")), (None, false));
    assert_eq!(h.s.claim(lane, None), (None, false));
    // A volume that ends frees a slot of its session that nothing refills.
    h.s.handle(Msg::Event {
        pid: "local".into(),
        event: Event::VolumeFailed {
            sid: sid.clone(),
            id: held[0].1.clone(),
            error: "the page pipeline failed".into(),
        },
    });
    let sent = drain_rx(&mut local);
    assert!(volumes(&sent).is_empty(), "{sent:?}");
    assert!(
        h.s.sessions[&sid].draining.is_some(),
        "{:?}",
        h.s.sessions[&sid]
    );
}

/// A local claim's archive fetched from a cloud library for a session that is gone
/// (this server's OCR was turned off meanwhile) names a job that may be another
/// machine's claim by then: the late fetch leaves that claim alone. It used to give it
/// back to the queue, and the processor's result was then thrown away as late.
#[test]
fn a_late_cloud_fetch_leaves_another_machines_claim_alone() {
    let mut h = harness(vec![primary()]);
    let cbz = h.add("A/V1.cbz", 4);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let sent = p.drain();
    assert_eq!(names(&volumes(&sent)), ["A/V1.cbz"]);
    let job = Job::new("A/V1.cbz", "g-1");
    let hold = bunko_server::ocr::sched::SpoolHold(Arc::new(bunko_store::LocalPath::InPlace(cbz)));
    let late = job.clone();
    h.run(move |s| s.spooled("a-session-that-ended", &late, Ok(hold)));
    assert_eq!(
        h.s.claims.get(&job).map(|c| c.pid.clone()),
        Some(p.pid.clone()),
        "the processor's claim was given back"
    );
    let late = job.clone();
    h.run(move |s| s.spooled("a-session-that-ended", &late, Err("gone".into())));
    assert_eq!(
        h.s.claims.get(&job).map(|c| c.pid.clone()),
        Some(p.pid.clone()),
        "the processor's claim was given back"
    );
}

/// A benchmark on this server cut short by turning its OCR off says so: it failed with
/// "local disconnected", the internal machine id, as if a processor had dropped off.
#[test]
fn a_benchmark_cut_short_by_turning_this_servers_ocr_off_says_so() {
    let mut h = harness(vec![primary()]);
    h.s.settings.autobench = true;
    h.s.settings.local_processing = true;
    h.add("A/V1.cbz", 6);
    h.at(0.0);
    let mut ops = local_up(&mut h, &["fp32"]);
    h.at(0.0);
    let op = bench_op(&drain_rx(&mut ops)).expect("an autobench of this server");

    set_local_processing(&mut h, false);

    let got = h.s.bench_get("g-1", "local");
    assert_eq!(got["state"], "failed", "{got}");
    assert_eq!(got["error"], "OCR on this machine was turned off", "{got}");
    let sample = h
        .storage()
        .join(".processing")
        .join(format!("{}.cbz", op.bid));
    assert!(!sample.exists(), "its sample is removed");
}
