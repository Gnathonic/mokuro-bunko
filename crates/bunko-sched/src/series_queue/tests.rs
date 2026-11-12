use super::*;

const T0: f64 = 1_794_398_400.0;

fn row(id: &str, primary: bool) -> RowDesc {
    RowDesc {
        kind: if primary { "ocr" } else { "layer" }.into(),
        id: id.into(),
        parts: json!([{"role": "recognizer", "name": id}]),
    }
}

fn q(row: &str, e: Option<f64>) -> LiveJob {
    LiveJob {
        row: row.into(),
        state: LiveState::Queued { e },
    }
}

/// `(stem, jobs)` of one volume.
type Vol<'a> = (&'a str, Vec<LiveJob>);

fn snap(now: f64, series: &[(&str, &[Vol<'_>])]) -> Snapshot {
    Snapshot {
        now,
        rows: vec![row("main", true), row("lines", false)],
        held: None,
        held_rows: BTreeSet::new(),
        series: series
            .iter()
            .map(|(folder, vols)| {
                (
                    folder.to_lowercase(),
                    SeriesLive {
                        folder: folder.to_string(),
                        vols: vols
                            .iter()
                            .map(|(stem, jobs)| (stem.to_string(), jobs.clone()))
                            .collect(),
                    },
                )
            })
            .collect(),
        finished: BTreeMap::new(),
    }
}

fn always(_: &str) -> bool {
    true
}

/// A queue of `n` series with `per` volumes each, waiting jobs finishing one every
/// `step` seconds from `start` (round-robin across series, as `order_jobs` does).
fn queue(now: f64, n: usize, per: usize, start: f64, step: f64) -> Snapshot {
    let mut s = Snapshot {
        now,
        rows: vec![row("main", true)],
        ..Default::default()
    };
    for v in 0..per {
        for k in 0..n {
            let e = start + step * (v * n + k) as f64;
            let folder = format!("Series {k:03}");
            s.series
                .entry(folder.to_lowercase())
                .or_insert_with(|| SeriesLive {
                    folder: folder.clone(),
                    vols: BTreeMap::new(),
                })
                .vols
                .insert(format!("{folder} v{v:02}"), vec![q("main", Some(e))]);
        }
    }
    s
}

/// The same queue with every predicted finish moved by `f(e)`.
fn moved(s: &Snapshot, now: f64, f: impl Fn(f64) -> f64) -> Snapshot {
    let mut s = s.clone();
    s.now = now;
    for l in s.series.values_mut() {
        for jobs in l.vols.values_mut() {
            for j in jobs {
                if let LiveState::Queued { e: Some(e) } = &mut j.state {
                    *e = f(*e);
                }
            }
        }
    }
    s
}

#[test]
fn tol_table() {
    assert_eq!(tol(0.0), 60.0);
    assert_eq!(tol(-500.0), 60.0);
    assert_eq!(tol(240.0), 60.0, "4 min");
    assert_eq!(tol(3600.0), 900.0, "1 h");
    assert_eq!(tol(360_000.0), 90_000.0, "100 h");
}

#[test]
fn clock_identity_interpolates_and_extrapolates() {
    let c = Clock::identity(T0 as i64);
    assert_eq!(
        c.knots,
        vec![
            (T0 as i64, T0 as i64),
            (T0 as i64 + 86_400, T0 as i64 + 86_400)
        ]
    );
    assert_eq!(c.at, T0 as i64);
    for x in [T0 - 5000.0, T0, T0 + 100.0, T0 + 200_000.0] {
        assert_eq!(c.eta(x), x);
        assert_eq!(c.position(x), x);
    }
    // Three knots: two slopes, the ends extrapolated.
    let c = Clock::from_json(&json!([[0, 100], [100, 300], [200, 350]]), 0).unwrap();
    assert_eq!(c.eta(50.0), 200.0);
    assert_eq!(c.eta(150.0), 325.0);
    assert_eq!(c.eta(-50.0), 0.0, "the first slope beyond the start");
    assert_eq!(c.eta(300.0), 400.0, "the last slope beyond the end");
    for w in [-50.0, 0.0, 50.0, 100.0, 150.0, 300.0] {
        assert!((c.position(c.eta(w)) - w).abs() < 1e-9, "{w}");
    }
}

#[test]
fn clock_json_is_checked() {
    let ok = Clock::from_json(&json!([[1, 2], [3, 4]]), 9).unwrap();
    assert_eq!(ok.knots_json(), json!([[1, 2], [3, 4]]));
    assert!(Clock::from_json(&json!([[1, 2]]), 0).is_none(), "one knot");
    assert!(
        Clock::from_json(&json!([[1, 2], [1, 4]]), 0).is_none(),
        "w not increasing"
    );
    assert!(
        Clock::from_json(&json!([[1, 2], [3, 2]]), 0).is_none(),
        "t not increasing"
    );
    assert!(Clock::from_json(&json!([[1, 2], [3, "4"]]), 0).is_none());
    let many: Vec<Value> = (0..17).map(|i| json!([i, i])).collect();
    assert!(
        Clock::from_json(&Value::Array(many), 0).is_none(),
        "17 knots"
    );
}

#[test]
fn fit_recovers_a_line_and_clamps_its_slope() {
    let now = T0;
    let pts: Vec<(f64, f64)> = (0..10)
        .map(|i| {
            let w = T0 + 600.0 * i as f64;
            (w, now + 1.2 * (w - now) + 300.0)
        })
        .collect();
    let c = Clock::fit(&pts, (T0, T0 + 5400.0), now).unwrap();
    for (w, e) in &pts {
        assert!((c.eta(*w) - e).abs() <= 1.0, "{w}: {} vs {e}", c.eta(*w));
    }
    assert_eq!(c.at, now as i64);
    // Too steep, too flat: clamped.
    let steep: Vec<(f64, f64)> = vec![(0.0, 0.0), (10.0, 1000.0)];
    let c = Clock::fit(&steep, (0.0, 10.0), 0.0).unwrap();
    assert!(((c.knots[1].1 - c.knots[0].1) as f64 / 10.0 - SLOPE_MAX).abs() < 0.2);
    let flat: Vec<(f64, f64)> = vec![(0.0, 0.0), (10_000.0, 1.0)];
    let c = Clock::fit(&flat, (0.0, 10_000.0), 0.0).unwrap();
    assert!(((c.knots[1].1 - c.knots[0].1) as f64 / 10_000.0 - SLOPE_MIN).abs() < 1e-3);
    // Fewer than 2 points: no fit.
    assert!(Clock::fit(&[(0.0, 0.0)], (0.0, 1.0), 0.0).is_none());
    // One position: still two increasing knots.
    let c = Clock::fit(&[(5.0, 5.0), (5.0, 7.0)], (5.0, 5.0), 0.0).unwrap();
    assert!(c.knots[1].0 > c.knots[0].0 && c.knots[1].1 > c.knots[0].1);
}

#[test]
fn render_shows_every_state() {
    let mut s = snap(
        T0,
        &[(
            "Example Saga",
            &[
                (
                    "Example Saga 03",
                    vec![LiveJob {
                        row: "main".into(),
                        state: LiveState::Running {
                            t0: T0 - 60.5,
                            e: Some(T0 + 359.0),
                        },
                    }],
                ),
                (
                    "Example Saga 04",
                    vec![q("main", Some(T0 + 1800.0)), q("lines", Some(T0 + 9000.0))],
                ),
                (
                    "Example Saga 05",
                    vec![LiveJob {
                        row: "main".into(),
                        state: LiveState::Claimed,
                    }],
                ),
                ("Example Saga 06", vec![q("main", None)]),
            ],
        )],
    );
    s.held_rows.insert("lines".into());
    let clock = Clock::identity(T0 as i64);
    let live = s.series.get("example saga");
    assert_eq!(
        classify(None, live, &s, true, &clock, false),
        Some(Class::Hard)
    );
    let p = render(None, live, &s, true, &clock, 7);
    let body = Value::Object(p.body("inst"));
    assert_eq!(
        body,
        json!({"v": 2, "instance_id": "inst", "rev": 7, "at": T0 as i64, "clock_at": T0 as i64,
            "series": "Example Saga",
            "rows": [row("main", true).to_json(), row("lines", false).to_json()],
            "vols": {
                "Example Saga 03": [[0, "r", T0 as i64 - 61, T0 as i64 + 360]],
                "Example Saga 04": [[0, "q", T0 as i64 + 1800], [1, "q", null]],
                "Example Saga 05": [[0, "c"]],
                "Example Saga 06": [[0, "q", null]]},
            "done": {}})
    );
    // Keys in byte order, whatever order they came in.
    let text = serde_json::to_string(&body).unwrap();
    assert!(text.find("Saga 03").unwrap() < text.find("Saga 06").unwrap());
    // Only the rows the file refers to.
    let only = snap(T0, &[("B", &[("B 1", vec![q("lines", None)])])]);
    let p = render(None, only.series.get("b"), &only, true, &clock, 1);
    assert_eq!(p.rows, vec![row("lines", false)]);
    assert_eq!(p.body("i")["vols"], json!({"B 1": [[0, "q", null]]}));
}

#[test]
fn a_held_row_keeps_its_published_position() {
    let clock = Clock::identity(T0 as i64);
    let s = snap(T0, &[("A", &[("A 1", vec![q("lines", Some(T0 + 600.0))])])]);
    let p = render(None, s.series.get("a"), &s, true, &clock, 1);
    assert_eq!(p.vols["A 1"][0].1, Tuple::Queued(Some(T0 as i64 + 600)));
    // The row is held: its ETA is hidden and moves, the file does not.
    let mut later = snap(
        T0 + 50.0,
        &[("A", &[("A 1", vec![q("lines", Some(T0 + 99_000.0))])])],
    );
    later.held_rows.insert("lines".into());
    assert_eq!(
        classify(Some(&p), later.series.get("a"), &later, true, &clock, true),
        None
    );
    let again = render(Some(&p), later.series.get("a"), &later, true, &clock, 2);
    assert_eq!(again.vols["A 1"][0].1, Tuple::Queued(Some(T0 as i64 + 600)));
    // Untimed while held: still no change.
    let mut untimed = later.clone();
    untimed
        .series
        .get_mut("a")
        .unwrap()
        .vols
        .get_mut("A 1")
        .unwrap()[0] = q("lines", None);
    assert_eq!(
        classify(
            Some(&p),
            untimed.series.get("a"),
            &untimed,
            true,
            &clock,
            true
        ),
        None
    );
}

#[test]
fn state_changes_are_p1_and_a_claim_is_none() {
    let clock = Clock::identity(T0 as i64);
    let s = snap(T0, &[("A", &[("A 1", vec![q("main", Some(T0 + 600.0))])])]);
    let p = render(None, s.series.get("a"), &s, true, &clock, 1);
    let with = |state: LiveState| {
        snap(
            T0 + 10.0,
            &[(
                "A",
                &[(
                    "A 1",
                    vec![LiveJob {
                        row: "main".into(),
                        state,
                    }],
                )],
            )],
        )
    };
    let claimed = with(LiveState::Claimed);
    assert_eq!(
        classify(
            Some(&p),
            claimed.series.get("a"),
            &claimed,
            true,
            &clock,
            true
        ),
        None
    );
    let running = with(LiveState::Running {
        t0: T0,
        e: Some(T0 + 600.0),
    });
    assert_eq!(
        classify(
            Some(&p),
            running.series.get("a"),
            &running,
            true,
            &clock,
            false
        ),
        Some(Class::State)
    );
    let untimed = with(LiveState::Queued { e: None });
    assert_eq!(
        classify(
            Some(&p),
            untimed.series.get("a"),
            &untimed,
            true,
            &clock,
            false
        ),
        Some(Class::State)
    );
    // Released: r → q.
    let p2 = render(Some(&p), running.series.get("a"), &running, true, &clock, 2);
    let back = with(LiveState::Queued {
        e: Some(T0 + 600.0),
    });
    assert_eq!(
        classify(Some(&p2), back.series.get("a"), &back, true, &clock, false),
        Some(Class::State)
    );
}

#[test]
fn drift_is_judged_only_after_a_clock_switch() {
    let clock = Clock::identity(T0 as i64);
    let s = snap(T0, &[("A", &[("A 1", vec![q("main", Some(T0 + 3600.0))])])]);
    let p = render(None, s.series.get("a"), &s, true, &clock, 1);
    // 1 h left: tol 900 s.
    let near = moved(&s, T0, |e| e + 800.0);
    assert_eq!(
        classify(Some(&p), near.series.get("a"), &near, true, &clock, true),
        None
    );
    let far = moved(&s, T0, |e| e + 1000.0);
    assert_eq!(
        classify(Some(&p), far.series.get("a"), &far, true, &clock, false),
        None
    );
    assert_eq!(
        classify(Some(&p), far.series.get("a"), &far, true, &clock, true),
        Some(Class::Drift)
    );
}

#[test]
fn a_running_job_drifts_only_by_overrunning() {
    let clock = Clock::identity(T0 as i64);
    let run = |e: f64| {
        snap(
            T0,
            &[(
                "A",
                &[(
                    "A 1",
                    vec![LiveJob {
                        row: "main".into(),
                        state: LiveState::Running { t0: T0, e: Some(e) },
                    }],
                )],
            )],
        )
    };
    let s = run(T0 + 1200.0);
    let p = render(None, s.series.get("a"), &s, true, &clock, 1);
    // Published run time 1200 s: allowed overrun max(300, 600) = 600.
    for (e, want) in [
        (T0 + 600.0, None),
        (T0 + 1790.0, None),
        (T0 + 1900.0, Some(Class::Drift)),
    ] {
        let live = run(e);
        assert_eq!(
            classify(Some(&p), live.series.get("a"), &live, true, &clock, true),
            want,
            "{e}"
        );
    }
}

#[test]
fn the_settle_rule_keeps_a_finished_job_running_until_settled() {
    let s = snap(
        T0,
        &[(
            "A",
            &[(
                "A 1",
                vec![LiveJob {
                    row: "main".into(),
                    state: LiveState::Running {
                        t0: T0,
                        e: Some(T0 + 120.0),
                    },
                }],
            )],
        )],
    );
    let mut sink = Sink::new(0, None);
    sink.refresh(&s, &always);
    // Finished: nothing owed any more, the series' write still pending.
    let after = snap(T0 + 600.0, &[]);
    let r = sink.refresh(&after, &|_| false);
    assert!(r.written.is_empty(), "{r:?}");
    let p = &sink.series["a"];
    assert_eq!(
        p.vols["A 1"][0].1,
        Tuple::Running {
            t0: T0 as i64,
            e: Some(T0 as i64 + 120)
        }
    );
    assert!(p.done.is_empty());
    // Settled: the job goes, with a done entry.
    let r = sink.refresh(&after, &always);
    assert_eq!(r.written, vec![("a".to_string(), Class::Hard)]);
    let p = &sink.series["a"];
    assert!(p.vols.is_empty());
    assert_eq!(p.done["A 1"], T0 as i64 + 600);
    // A job finished while it was still published waiting is shown running, from now.
    let s = snap(T0, &[("B", &[("B 1", vec![q("main", Some(T0 + 600.0))])])]);
    let mut sink = Sink::new(0, None);
    sink.refresh(&s, &always);
    let r = sink.refresh(&snap(T0 + 30.0, &[]), &|_| false);
    assert_eq!(r.written, vec![("b".to_string(), Class::State)]);
    assert_eq!(
        sink.series["b"].vols["B 1"][0].1,
        Tuple::Running {
            t0: T0 as i64 + 30,
            e: Some(T0 as i64 + 30)
        }
    );
}

/// `snap` plus volumes finished lately: `(folder, stem, when)`.
fn with_finished(mut s: Snapshot, finished: &[(&str, &str, f64)]) -> Snapshot {
    for (folder, stem, at) in finished {
        s.finished
            .entry(folder.to_lowercase())
            .or_insert_with(|| (folder.to_string(), BTreeMap::new()))
            .1
            .insert(stem.to_string(), *at);
    }
    s
}

#[test]
fn a_job_queued_and_finished_between_two_looks_still_leaves_a_done_entry() {
    // Regression (rig S4/S6): a volume OCR'd faster than one write window never
    // reached a file, so no `done` entry ever told a reader it finished.
    let mut sink = Sink::new(0, None);
    assert!(sink.refresh(&snap(T0, &[]), &always).written.is_empty());
    let s = with_finished(snap(T0 + 30.0, &[]), &[("A", "A 1", T0 + 20.0)]);
    // Its series is not settled yet (its sidecar is on its way): nothing yet.
    let r = sink.refresh(&s, &|_| false);
    assert!(r.written.is_empty(), "{r:?}");
    // Settled: a file naming it done, and listing nothing.
    let r = sink.refresh(&s, &always);
    assert_eq!(r.written, vec![("a".to_string(), Class::Hard)]);
    let p = sink.series["a"].clone();
    assert_eq!(p.folder, "A");
    assert!(p.vols.is_empty() && p.rows.is_empty());
    assert_eq!(p.done["A 1"], T0 as i64 + 30);
    // Once is enough: the same finish is never written again …
    for later in [60.0, 600.0, 3600.0] {
        let s = with_finished(snap(T0 + later, &[]), &[("A", "A 1", T0 + 20.0)]);
        let r = sink.refresh(&s, &always);
        assert!(r.written.is_empty(), "{later}: {r:?}");
    }
    // … and a file that lists other jobs gains it too, in one write.
    let s = snap(T0, &[("B", &[("B 2", vec![q("main", None)])])]);
    let mut sink = Sink::new(0, None);
    sink.refresh(&s, &always);
    let s = with_finished(
        snap(T0 + 30.0, &[("B", &[("B 2", vec![q("main", None)])])]),
        &[("B", "B 1", T0 + 25.0)],
    );
    let r = sink.refresh(&s, &always);
    assert_eq!(r.written, vec![("b".to_string(), Class::Hard)]);
    let p = &sink.series["b"];
    assert_eq!(p.done["B 1"], T0 as i64 + 30);
    assert!(p.vols.contains_key("B 2") && !p.vols.contains_key("B 1"));
    assert_eq!(p.rows.len(), 1, "no row for the unshown job");
    assert!(sink.refresh(&s, &always).written.is_empty());
}

#[test]
fn an_unshown_finish_is_not_one_the_file_already_knows() {
    let clock = Clock::identity(T0 as i64);
    // Still listed: the normal path (settle, then done) has it.
    let s = with_finished(
        snap(T0, &[("A", &[("A 1", vec![q("main", None)])])]),
        &[("A", "A 1", T0)],
    );
    assert!(s.unshown_finishes("a", None, &clock).is_none());
    let p = render(None, s.series.get("a"), &s, true, &clock, 1);
    let s = with_finished(snap(T0 + 10.0, &[]), &[("A", "A 1", T0 + 5.0)]);
    assert!(
        s.unshown_finishes("a", Some(&p), &clock).is_none(),
        "published: the settle path"
    );
    // Named done since it finished: nothing; finished again after it: again.
    let mut p = p.clone();
    p.vols.clear();
    p.done.insert("A 1".into(), T0 as i64 + 8);
    assert!(s.unshown_finishes("a", Some(&p), &clock).is_none());
    let again = with_finished(snap(T0 + 99.0, &[]), &[("A", "A 1", T0 + 90.0)]);
    assert!(again.unshown_finishes("a", Some(&p), &clock).is_some());
    // Older than a day, or than the oldest of a full tail: dropped for good.
    let old = with_finished(snap(T0 + 90_000.0, &[]), &[("A", "A 2", T0)]);
    assert!(old.unshown_finishes("a", None, &clock).is_none());
    let mut full = p.clone();
    full.done = (0..DONE_MAX as i64)
        .map(|i| (format!("X {i}"), T0 as i64 + 100 + i))
        .collect();
    let s = with_finished(snap(T0 + 500.0, &[]), &[("A", "A 3", T0 + 100.0)]);
    assert!(s.unshown_finishes("a", Some(&full), &clock).is_none());
    let s = with_finished(snap(T0 + 500.0, &[]), &[("A", "A 3", T0 + 400.0)]);
    assert!(s.unshown_finishes("a", Some(&full), &clock).is_some());
}

#[test]
fn the_done_tail_keeps_a_day_and_the_newest_64() {
    let clock = Clock::identity(T0 as i64);
    let stems: Vec<String> = (0..70).map(|i| format!("A {i:03}")).collect();
    let full: Vec<(&str, Vec<LiveJob>)> = stems
        .iter()
        .map(|s| (s.as_str(), vec![q("main", None)]))
        .collect();
    let s = snap(T0, &[("A", &full)]);
    let mut p = render(None, s.series.get("a"), &s, true, &clock, 1);
    // Finish them one a minute.
    for i in 0..70 {
        let rest: Vec<(&str, Vec<LiveJob>)> = full[i + 1..].to_vec();
        let s = snap(T0 + 60.0 * (i + 1) as f64, &[("A", &rest)]);
        p = render(Some(&p), s.series.get("a"), &s, true, &clock, 2 + i as u64);
    }
    assert_eq!(p.done.len(), DONE_MAX);
    assert!(!p.done.contains_key("A 005") && p.done.contains_key("A 006"));
    // A day later, one more write drops them all; then the file can go.
    let s = snap(
        T0 + 100_000.0,
        &[("A", &[("A 999", vec![q("main", None)])])],
    );
    let p = render(Some(&p), s.series.get("a"), &s, true, &clock, 99);
    assert!(p.done.is_empty());
    let empty = snap(T0 + 200_000.0, &[]);
    let p = render(Some(&p), None, &empty, true, &clock, 100);
    assert_eq!(p.done.len(), 1);
    assert!(!p.expired(T0 + 200_000.0));
    assert!(p.expired(T0 + 200_000.0 + 86_400.0));
}

#[test]
fn time_alone_writes_nothing() {
    // Finding 1: the cloud copy was rewritten on every floor tick.
    let s = queue(T0, 30, 4, T0 + 600.0, 120.0);
    let mut sink = Sink::new(0, Some(REFIT_SECONDS));
    assert_eq!(sink.refresh(&s, &always).written.len(), 30);
    for minute in 1..=10 {
        let later = moved(&s, T0 + 60.0 * minute as f64, |e| e);
        let r = sink.refresh(&later, &always);
        assert!(r.written.is_empty(), "minute {minute}: {r:?}");
    }
}

#[test]
fn a_fleet_speed_change_or_a_pause_moves_only_the_clock() {
    let s = queue(T0, 40, 5, T0 + 300.0, 90.0);
    let mut sink = Sink::new(0, Some(REFIT_SECONDS));
    sink.refresh(&s, &always);
    // 20 % slower, seen at the next refit.
    let now = T0 + 300.0;
    let slower = moved(&s, now, |e| now + 1.2 * (e - T0));
    let r = sink.refresh(&slower, &always);
    assert!(r.refit, "{r:?}");
    assert!(r.written.is_empty(), "{:?}", r.written);
    let clock = sink.clock.clone().unwrap();
    for (key, p) in &sink.series {
        for (stem, jobs) in &p.vols {
            let Tuple::Queued(Some(w)) = jobs[0].1 else {
                panic!()
            };
            let LiveState::Queued { e: Some(e) } = slower.series[key].vols[stem][0].state else {
                panic!()
            };
            let shown = clock.eta(w as f64);
            assert!(
                (shown - e).abs() <= tol(shown - now),
                "{stem}: {shown} vs {e}"
            );
        }
    }
    // A 2 h pause, then unheld: every finish 2 h later.
    let mut paused = moved(&slower, now + 3600.0, |e| e);
    paused.held = Some("paused".into());
    let r = sink.refresh(&paused, &always);
    assert!(!r.refit && r.written.is_empty(), "{r:?}");
    let resumed = moved(&slower, now + 7200.0 + 300.0, |e| e + 7200.0 + 300.0);
    let r = sink.refresh(&resumed, &always);
    assert!(r.refit, "{r:?}");
    assert!(r.written.is_empty(), "{:?}", r.written);
}

#[test]
fn a_new_row_rewrites_every_series_once() {
    let s = queue(T0, 10, 3, T0 + 600.0, 60.0);
    let mut sink = Sink::new(0, Some(REFIT_SECONDS));
    sink.refresh(&s, &always);
    let mut more = s.clone();
    more.rows.push(row("lines", false));
    for l in more.series.values_mut() {
        for jobs in l.vols.values_mut() {
            jobs.push(q("lines", None));
        }
    }
    let r = sink.refresh(&more, &always);
    assert_eq!(r.written.len(), 10);
    assert!(r.written.iter().all(|(_, c)| *c == Class::Hard));
    assert!(sink.refresh(&more, &always).written.is_empty());
    // A new engine version in a row's parts: the rows table changed.
    let mut newer = more.clone();
    newer.rows[1].parts = json!([{"role": "recognizer", "name": "lines", "version": "2"}]);
    let r = sink.refresh(&newer, &always);
    assert_eq!(r.written.len(), 10);
}

#[test]
fn a_series_that_owes_nothing_is_dropped_once_its_done_tail_expires() {
    let s = snap(T0, &[("A", &[("A 1", vec![q("main", None)])])]);
    let mut sink = Sink::new(5, None);
    sink.refresh(&s, &always);
    assert_eq!(sink.series["a"].rev, 6, "revisions continue from the base");
    sink.refresh(&snap(T0 + 10.0, &[]), &always);
    assert!(sink.series.contains_key("a"));
    let r = sink.refresh(&snap(T0 + 10.0 + 86_400.0, &[]), &always);
    assert_eq!(r.dropped, vec!["a".to_string()]);
}

#[test]
fn the_live_block() {
    let mut s = snap(T0, &[]);
    s.held = Some("no-processor".into());
    s.held_rows.insert("lines".into());
    let c = Clock::identity(T0 as i64);
    assert_eq!(
        live_block(&s, &c, 5),
        json!({"held": {"reason": "no-processor"}, "held_rows": ["lines"],
               "clock": [[T0 as i64, T0 as i64], [T0 as i64 + 86_400, T0 as i64 + 86_400]],
               "clock_at": T0 as i64, "batch_s": 5})
    );
}

#[test]
fn the_file_name_is_the_shared_one() {
    assert_eq!(FILE_NAME, bunko_core::names::SERIES_QUEUE_FILE);
}

#[test]
fn the_marker_block_in_spec_order() {
    let mut s = snap(T0, &[]);
    s.held_rows.insert("lines".into());
    let c = Clock::identity(T0 as i64);
    let b = marker_block(Some(&s), &c, 120);
    let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "v",
            "file",
            "batch_s",
            "held",
            "held_rows",
            "clock",
            "clock_at"
        ]
    );
    assert_eq!(b["v"], 2);
    assert_eq!(b["file"], ".mokuro-series-queue.json");
    assert_eq!(b["held"], Value::Null);
    assert_eq!(b["held_rows"], json!(["lines"]));
    // Without a snapshot: nothing held.
    assert_eq!(marker_block(None, &c, 300)["held_rows"], json!([]));
}

#[test]
fn a_shifted_snapshot_moves_every_time_and_nothing_else() {
    let mut s = snap(
        T0,
        &[(
            "A",
            &[
                ("A 1", vec![q("main", Some(T0 + 600.0)), q("lines", None)]),
                (
                    "A 2",
                    vec![LiveJob {
                        row: "main".into(),
                        state: LiveState::Running {
                            t0: T0 - 60.0,
                            e: Some(T0 + 60.0),
                        },
                    }],
                ),
            ],
        )],
    );
    s.series.get_mut("a").unwrap().vols.insert(
        "A 3".into(),
        vec![LiveJob {
            row: "main".into(),
            state: LiveState::Claimed,
        }],
    );
    let s = with_finished(s, &[("A", "A 0", T0 - 30.0)]);
    let m = s.shifted(-100.0);
    assert_eq!(m.now, T0 - 100.0);
    assert_eq!(m.finished["a"].1["A 0"], T0 - 130.0);
    let a = &m.series["a"];
    assert_eq!(
        a.vols["A 1"][0].state,
        LiveState::Queued {
            e: Some(T0 + 500.0)
        }
    );
    assert_eq!(a.vols["A 1"][1].state, LiveState::Queued { e: None });
    assert_eq!(
        a.vols["A 2"][0].state,
        LiveState::Running {
            t0: T0 - 160.0,
            e: Some(T0 - 40.0)
        }
    );
    assert_eq!(a.vols["A 3"][0].state, LiveState::Claimed);
    assert_eq!(s.shifted(0.0), s);
    assert_eq!(a.soonest(), Some(T0 - 40.0));
}

#[test]
fn the_backlog_orders_p0_and_p1_by_age_p2_by_soonest_finish_then_p3() {
    let mut b: Backlog<&str> = Backlog::default();
    b.mark("drift-late", Priority::P2, T0, Some(T0 + 900.0));
    b.mark("tidy", Priority::P3, T0, None);
    b.mark("state", Priority::P1, T0 + 1.0, None);
    b.mark("hard-new", Priority::P0, T0 + 2.0, None);
    b.mark("drift-soon", Priority::P2, T0 + 3.0, Some(T0 + 60.0));
    b.mark("hard-old", Priority::P0, T0 + 1.5, None);
    b.mark("drift-untimed", Priority::P2, T0, None);
    assert_eq!(
        b.order(),
        [
            "hard-old",
            "hard-new",
            "state",
            "drift-soon",
            "drift-late",
            "drift-untimed",
            "tidy"
        ]
    );
    // Marks of one age (a take over marks every series at once): soonest first.
    let mut t: Backlog<&str> = Backlog::default();
    t.mark("far", Priority::P0, T0, Some(T0 + 9000.0));
    t.mark("untimed", Priority::P0, T0, None);
    t.mark("near", Priority::P0, T0, Some(T0 + 60.0));
    assert_eq!(t.order(), ["near", "far", "untimed"]);
    // A more urgent mark keeps its age; a less urgent one changes nothing.
    b.mark("state", Priority::P0, T0 + 50.0, None);
    assert_eq!(b.get(&"state").unwrap().since, T0 + 1.0);
    assert_eq!(b.order()[0], "state");
    b.mark("hard-old", Priority::P2, T0 + 60.0, None);
    assert_eq!(b.get(&"hard-old").unwrap().priority, Priority::P0);
    assert_eq!(b.unmark(&"tidy").map(|m| m.priority), Some(Priority::P3));
    assert_eq!(b.len(), 6);
}

#[test]
fn aging_promotes_p2_to_p1_to_p0_and_p3_to_p2() {
    let w = 120.0;
    let mut b: Backlog<&str> = Backlog::default();
    b.mark("drift", Priority::P2, T0, Some(T0 + 60.0));
    b.mark("tidy", Priority::P3, T0, None);
    b.age(T0 + 6.0 * w, w);
    assert_eq!(
        b.get(&"drift").unwrap().priority,
        Priority::P2,
        "not more than 6 W"
    );
    b.age(T0 + 6.0 * w + 1.0, w);
    assert_eq!(b.get(&"drift").unwrap().priority, Priority::P1);
    // Six more windows at P1 make it P0.
    b.age(T0 + 12.0 * w + 1.0, w);
    assert_eq!(b.get(&"drift").unwrap().priority, Priority::P1);
    b.age(T0 + 12.0 * w + 2.0, w);
    assert_eq!(b.get(&"drift").unwrap().priority, Priority::P0);
    assert_eq!(
        b.get(&"tidy").unwrap().priority,
        Priority::P3,
        "1 h has not passed"
    );
    b.age(T0 + 3601.0, w);
    assert_eq!(b.get(&"tidy").unwrap().priority, Priority::P2);
    // P0 stays.
    b.age(T0 + 100_000.0, w);
    assert_eq!(b.get(&"drift").unwrap().priority, Priority::P0);
}

#[test]
fn the_budget_halves_for_three_windows_then_recovers_by_one() {
    let mut b = Budget::new(30);
    assert_eq!(b.window(), 30);
    b.throttled();
    assert_eq!((0..3).map(|_| b.window()).collect::<Vec<_>>(), [15, 15, 15]);
    assert_eq!(b.window(), 16);
    assert_eq!(b.window(), 17);
    // A second signal halves what it is now.
    b.throttled();
    assert_eq!(b.window(), 8);
    for _ in 0..2 {
        b.window();
    }
    let rest: Vec<usize> = (0..30).map(|_| b.window()).collect();
    assert_eq!(rest[0], 9);
    assert_eq!(*rest.last().unwrap(), 30, "back to the cap, and no further");
    // Never below one.
    let mut one = Budget::new(1);
    one.throttled();
    assert_eq!(one.window(), 1);
}
