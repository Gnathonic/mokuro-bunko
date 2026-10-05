//! A processor's own pause (GUI.md §3, protocol v3 `availability` / `released`): the
//! scheduler stops offering it work, drains its sessions, takes released claims back at
//! once without recording anything, and never blames what ends while it is paused.

mod ocr_common;

use bunko_proto::{Availability, Event, Op};
use bunko_server::ocr::sched::{Msg, RegisterInput, RegisterOutcome};
use bunko_server::ocr::types::Job;
use ocr_common::*;
use serde_json::json;
use tokio::sync::mpsc;

fn paused(until: Option<&str>) -> Event {
    Event::Availability(Availability {
        paused: true,
        until: until.map(str::to_string),
        reason: Some("user".into()),
    })
}

fn resumed() -> Event {
    Event::Availability(Availability::default())
}

fn released(claims: &[&str]) -> Event {
    Event::Released {
        claims: claims.iter().map(|c| c.to_string()).collect(),
    }
}

fn closes(ops: &[Op], sid: &str) -> bool {
    ops.iter()
        .any(|o| matches!(o, Op::CloseSession { sid: s } if s == sid))
}

#[test]
fn pause_after_volume_finishes_the_running_one_and_returns_the_rest() {
    let mut h = harness(vec![primary()]);
    for rel in ["A/V1.cbz", "A/V2.cbz", "A/V3.cbz"] {
        h.add(rel, 2);
    }
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let v = volumes(&ops);
    assert_eq!(v.len(), 2, "the 2-deep lookahead: {ops:?}");
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &v[0].1, 2));
    // The processor pauses after this volume: the second claim never started.
    h.event(&p, paused(Some("2030-01-01T08:00:00Z")));
    h.event(&p, released(&[&v[1].1]));
    assert!(
        !h.s.claims.contains_key(&Job::new("A/V2.cbz", "g-1")),
        "the released claim is back in the queue"
    );
    assert!(h.s.failures.is_empty());
    let shown = h.s.processors();
    assert_eq!(
        shown[0]["pause"],
        json!({"paused": true, "until": "2030-01-01T08:00:00Z", "reason": "user"})
    );
    assert!(
        volumes(&p.drain()).is_empty(),
        "nothing more is offered to a paused processor"
    );
    // The running volume finishes and is installed; the session drains and closes.
    h.done(&p, &sid, &v[0].1, "V1.mokuro", 2, 1.0);
    assert!(h.library().join("A/V1.mokuro").is_file());
    let ops = p.drain();
    assert!(volumes(&ops).is_empty(), "{ops:?}");
    assert!(closes(&ops, &sid), "{ops:?}");
    h.event(&p, exit(&sid, Some(0)));
    assert!(h.s.sessions.is_empty());
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty());
    assert!(
        h.s.lanes.iter().all(|l| l.pid != p.pid),
        "a paused machine has no lanes"
    );
    assert_eq!(h.s.queue_hold(), Some("paused"));
    // A poll later: still nothing.
    h.at(60.0);
    assert!(opened(&p.drain()).is_empty());
    // Resumed: work flows again, starting with the released volume.
    h.event(&p, resumed());
    let ops = p.drain();
    let sid2 = opened(&ops)[0].clone();
    assert_ne!(sid2, sid);
    let names: Vec<String> = volumes(&ops).into_iter().map(|v| v.2).collect();
    assert!(
        names.iter().any(|n| n.ends_with("A/V2.cbz")),
        "the released volume is offered again: {names:?}"
    );
    assert_eq!(h.s.processors()[0]["pause"], serde_json::Value::Null);
}

#[test]
fn pause_now_requeues_without_failure_and_blames_nobody() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.add("B/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let v = volumes(&ops);
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &v[0].1, 2));
    h.event(
        &p,
        Event::Page {
            sid: sid.clone(),
            id: v[0].1.clone(),
            done: 1,
            total: 2,
        },
    );
    // Pause now: both claims back, then the abandoned session says only `exit`.
    h.event(&p, paused(None));
    h.event(&p, released(&[&v[0].1, &v[1].1]));
    assert!(h.s.claims.is_empty(), "{:?}", h.s.claims.keys());
    h.event(&p, exit(&sid, None));
    assert!(h.s.sessions.is_empty());
    assert!(h.s.failures.is_empty(), "{:?}", h.s.failures);
    assert!(h.s.strikes.is_empty(), "{:?}", h.s.strikes);
    assert!(h.s.start_backoff.is_empty());
    // Resume: the same volume is picked up again.
    h.event(&p, resumed());
    let ops = p.drain();
    assert_eq!(opened(&ops).len(), 1);
    let again: Vec<String> = volumes(&ops).into_iter().map(|v| v.2).collect();
    assert!(again.contains(&v[0].2), "{again:?} vs {:?}", v[0].2);
}

#[test]
fn a_session_ending_while_paused_is_nobodys_failure() {
    // The processor paused (now) but an exit crossed a volume op on the wire: the
    // session ends holding a claim the processor never saw. Released, not blamed.
    let mut h = harness(vec![primary()]);
    for rel in ["A/V1.cbz", "A/V2.cbz", "A/V3.cbz"] {
        h.add(rel, 2);
    }
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let ops = p.drain();
    let sid = opened(&ops)[0].clone();
    let v = volumes(&ops);
    h.event(&p, ready(&sid));
    h.event(&p, started(&sid, &v[0].1, 2));
    h.event(&p, paused(None));
    h.event(&p, released(&[&v[0].1]));
    h.event(
        &p,
        Event::Fatal {
            sid: sid.clone(),
            error: "cancelled".into(),
        },
    );
    h.event(&p, exit(&sid, Some(1)));
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty(), "{:?}", h.s.failures);
    assert!(h.s.strikes.is_empty());
    assert!(h.s.stopped.is_empty());
    // An `open_session` that crossed the pause is answered with a bare exit.
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let mut p = h.connect("box", 1);
    let sid = opened(&p.drain())[0].clone();
    h.event(&p, paused(None));
    h.event(&p, exit(&sid, None));
    h.event(&p, released(&["v1"]));
    assert!(h.s.claims.is_empty());
    assert!(h.s.failures.is_empty());
    assert!(
        h.s.start_backoff.is_empty(),
        "a paused runner did not fail to start"
    );
}

#[test]
fn a_processor_registering_paused_gets_no_work() {
    let mut h = harness(vec![primary()]);
    h.add("A/V1.cbz", 2);
    h.at(0.0);
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    h.s.handle(Msg::Register {
        input: RegisterInput {
            username: "acct-box".into(),
            body: json!({
                "protocol": 3,
                "name": "box",
                "catalog": {"engines": ["hayai-nova", "paddle-manga", "ppocr-manga"],
                            "detectors": ["ppocr-manga"],
                            "devices": [{"id": "cpu", "label": "CPU"}]},
                "availability": {"paused": true, "until": "2030-01-01T08:00:00Z", "reason": "schedule"},
            }),
            account_stamp: None,
        },
        reply,
    });
    let pid = match rx.try_recv().unwrap() {
        RegisterOutcome::Ok(r) => r.processor_id,
        other => panic!("{other:?}"),
    };
    let (tx, mut ops) = mpsc::unbounded_channel();
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    h.s.handle(Msg::SocketOpen {
        pid: pid.clone(),
        username: "acct-box".into(),
        ops: tx,
        reply,
    });
    rx.try_recv().unwrap().unwrap();
    assert!(
        ops.try_recv().is_err(),
        "a paused processor is offered nothing"
    );
    assert_eq!(h.s.queue_hold(), Some("paused"));
    assert_eq!(h.s.processors()[0]["pause"]["reason"], "schedule");
    // An admin cannot benchmark it either.
    let refused = h.s.bench_enqueue(bunko_server::ocr::sched::BenchRequest {
        key: "g-1".into(),
        spec: None,
        pages: None,
        processor: "box".into(),
        autobench: false,
        precision_only: false,
    });
    let (status, body) = refused.unwrap_err();
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("paused"), "{body}");
    // Another machine still works.
    let mut other = h.connect("other", 1);
    assert_eq!(opened(&other.drain()).len(), 1);
}
