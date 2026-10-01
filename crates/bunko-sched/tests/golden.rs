//! Differential tests: replay the fixtures `tests/golden/gen_golden.py`
//! generated with the 0.5.2 Python code and require the same answers.
//! Floats agree within 1e-9 relative; strings, ints, key order exactly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bunko_sched::congestion::{self, VolumeTiming};
use bunko_sched::eft::{EftLane, EftParams, earliest_finish_claim};
use bunko_sched::failures::{self, FailureStore, NewFailure};
use bunko_sched::job_order::{JobKey, Token, name_key, natural_key, order_jobs};
use bunko_sched::outlook::{OwedRow, pending_entries, recheck_after};
use bunko_sched::plan::{LanePricing, MachineBench, PlanInputs, plan_queue, progress_metrics};
use bunko_sched::py::{self, Object, as_float};
use bunko_sched::pyjson::dumps_indent2;
use bunko_sched::rate::{
    ManualClock, RateEstimate, RateModel, SESSION_ALPHA, StaticPriors, emission_rate,
    fit_volume_cost,
};
use bunko_sched::speed::{RowRef, busy_reason, speed_report};
use bunko_sched::throughput::{Sample, Throughput, profile_throughput, throughput_of};
use serde_json::{Value, json};

fn load(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Order-sensitive JSON comparison with a float tolerance.
fn same(path: &str, actual: &Value, expected: &Value) -> Result<(), String> {
    match (actual, expected) {
        (Value::Number(a), Value::Number(e)) => {
            let e_int = !e.is_f64();
            let a_int = !a.is_f64();
            if e_int != a_int {
                return Err(format!("{path}: int/float kind differs: {a} vs {e}"));
            }
            if e_int {
                if a.as_i64() == e.as_i64() && a.as_u64() == e.as_u64() {
                    return Ok(());
                }
                return Err(format!("{path}: {a} != {e}"));
            }
            let (x, y) = (a.as_f64().unwrap(), e.as_f64().unwrap());
            // BUNKO_GOLDEN_EXACT=1: require bit-identical floats (diagnostics).
            let tolerance = if std::env::var_os("BUNKO_GOLDEN_EXACT").is_some() {
                0.0
            } else {
                1e-9
            };
            if x == y || (x - y).abs() <= tolerance * x.abs().max(y.abs()) {
                Ok(())
            } else {
                Err(format!("{path}: {x} != {y}"))
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            if a.len() != e.len() {
                return Err(format!(
                    "{path}: length {} != {}\n  actual:   {actual}\n  expected: {expected}",
                    a.len(),
                    e.len()
                ));
            }
            for (i, (x, y)) in a.iter().zip(e).enumerate() {
                same(&format!("{path}[{i}]"), x, y)?;
            }
            Ok(())
        }
        (Value::Object(a), Value::Object(e)) => {
            let ak: Vec<&String> = a.keys().collect();
            let ek: Vec<&String> = e.keys().collect();
            if ak != ek {
                return Err(format!("{path}: keys {ak:?} != {ek:?}"));
            }
            for (k, v) in e {
                same(&format!("{path}.{k}"), &a[k], v)?;
            }
            Ok(())
        }
        _ if actual == expected => Ok(()),
        _ => Err(format!("{path}: {actual} != {expected}")),
    }
}

fn check(path: &str, actual: &Value, expected: &Value) {
    if let Err(e) = same(path, actual, expected) {
        panic!("{e}");
    }
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_owned()
}

fn opt_s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn est(e: Option<RateEstimate>) -> Value {
    e.map_or(Value::Null, |e| {
        json!({
            "pages_per_second": e.pages_per_second,
            "source": e.source,
            "volumes_observed": e.volumes_observed,
            "latency_seconds": e.latency_seconds,
        })
    })
}

fn tp(t: Option<Throughput>) -> Value {
    t.map_or(Value::Null, |t| json!({"pages": t.pages, "seconds": t.seconds, "volumes": t.volumes, "last_at": t.last_at}))
}

fn obj(v: &Value) -> Object {
    v.as_object().cloned().unwrap()
}

// --- natural keys -------------------------------------------------------------

fn encode_key(name: &str) -> Value {
    Value::Array(
        natural_key(name)
            .into_iter()
            .map(|t| match t {
                Token::Number { value, fraction } => json!([0, value.digits(), fraction]),
                Token::Text(text) => json!([1, "0", text]),
            })
            .collect(),
    )
}

#[test]
fn natural_key_matches_python() {
    let data = load("natural_key.json");
    let names = data["names"].as_array().unwrap();
    let keys = data["keys"].as_array().unwrap();
    for (name, key) in names.iter().zip(keys) {
        check(
            &format!("natural_key({name})"),
            &encode_key(name.as_str().unwrap()),
            key,
        );
    }
    for case in data["sorted"].as_array().unwrap() {
        let mut items: Vec<String> = case["input"].as_array().unwrap().iter().map(s).collect();
        items.sort_by_key(|n| name_key(n));
        check("sorted", &json!(items), &case["sorted"]);
    }
}

#[test]
fn order_jobs_matches_python() {
    for (i, case) in load("order_jobs.json")
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let jobs: Vec<Value> = case["jobs"].as_array().unwrap().clone();
        let rank: HashMap<String, usize> = case["rank"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_u64().unwrap() as usize))
            .collect();
        let served: HashMap<String, String> = case["last_served"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), s(v)))
            .collect();
        let order = order_jobs(
            jobs,
            &rank,
            |j| JobKey {
                series: s(&j[0]),
                volume: s(&j[1]),
                generation_id: s(&j[2]),
            },
            &served,
        );
        let idx: Vec<Value> = order.iter().map(|j| j[3].clone()).collect();
        check(
            &format!("order_jobs[{i}]"),
            &Value::Array(idx),
            &case["order"],
        );
    }
}

// --- rate model ------------------------------------------------------------------

#[test]
fn fit_and_emission_match_python() {
    for (i, case) in load("fit.json").as_array().unwrap().iter().enumerate() {
        if let Some(em) = case.get("emission") {
            let got = emission_rate(em[0].as_i64().unwrap(), f(&em[1]));
            check(&format!("emission[{i}]"), &json!(got), &case["rate"]);
            continue;
        }
        let samples: Vec<(f64, f64)> = case["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (f(&p[0]), f(&p[1])))
            .collect();
        let got = fit_volume_cost(&samples, f(&case["alpha"]))
            .map(|fit| json!([fit.latency_seconds, fit.seconds_per_page]));
        check(
            &format!("fit[{i}]"),
            &got.unwrap_or(Value::Null),
            &case["fit"],
        );
    }
}

fn build_world(world: &Value) -> (Arc<ManualClock>, RateModel) {
    let clock = Arc::new(ManualClock::new(f(&world["start"])));
    let priors = Arc::new(StaticPriors::new(
        obj(&world["congestion"]),
        obj(&world["bench"]),
    ));
    let rm = RateModel::new(clock.clone(), priors, SESSION_ALPHA);
    for op in world["ops"].as_array().unwrap() {
        clock.set(f(&op["t"]));
        match op["op"].as_str().unwrap() {
            "volume" => rm.record_volume_values(
                op["key"].as_str().unwrap(),
                op.get("pages"),
                op.get("seconds"),
                op["first"].as_bool().unwrap(),
            ),
            "startup" => {
                if let Some(v) = as_float(op.get("seconds")) {
                    rm.record_startup(op["key"].as_str().unwrap(), v);
                }
            }
            _ => rm.forget(op["gen"].as_str().unwrap()),
        }
    }
    clock.set(f(&world["now"]));
    (clock, rm)
}

#[test]
fn rate_model_matches_python() {
    for (w, world) in load("rate_model.json")
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (_clock, rm) = build_world(world);
        for (qi, q) in world["queries"].as_array().unwrap().iter().enumerate() {
            let gen_id = q.get("gen").and_then(Value::as_str).unwrap_or("");
            let path = format!("world[{w}].query[{qi}]={}", q["q"]);
            let got = match q["q"].as_str().unwrap() {
                "rate" => est(rm.rate(gen_id, q["op"].as_i64().unwrap(), f(&q["os"]))),
                "rate_on" => {
                    let prior = q["prior"].as_f64().map(RateEstimate::bench);
                    est(rm.rate_on(
                        gen_id,
                        q["key"].as_str(),
                        prior.as_ref(),
                        q["op"].as_i64().unwrap(),
                        f(&q["os"]),
                    ))
                }
                "startup" => {
                    let st = rm.startup(gen_id);
                    json!({"seconds": st.seconds, "source": st.source, "rough": st.rough})
                }
                "startup_on" => {
                    let st = rm.startup_on(gen_id, q["key"].as_str(), q["prior"].as_f64());
                    json!({"seconds": st.seconds, "source": st.source, "rough": st.rough})
                }
                "latency" => json!(rm.latency(gen_id)),
                "throughput" => tp(rm.throughput(q["key"].as_str().unwrap())),
                "evidence" => json!(rm.machines_with_evidence(gen_id, f(&q["within"]))),
                "report" => rm.report(gen_id),
                other => panic!("unknown query {other}"),
            };
            check(&path, &got, &q["out"]);
        }
    }
}

// --- earliest finish ---------------------------------------------------------------

#[test]
fn earliest_finish_claim_matches_python() {
    for (i, case) in load("eft.json").as_array().unwrap().iter().enumerate() {
        let lanes: Vec<EftLane<i64>> = case["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| EftLane {
                key: l[0].as_i64().unwrap(),
                machine: s(&l[1]),
                free_in: f(&l[2]),
                warm: opt_s(&l[3]),
                rows: l[4].as_array().unwrap().iter().map(s).collect(),
            })
            .collect();
        let rates: HashMap<(String, String), Option<(f64, f64)>> = case["rates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    (s(&r[0]), s(&r[1])),
                    r[2].as_array().map(|p| (f(&p[0]), f(&p[1]))),
                )
            })
            .collect();
        let startups: HashMap<(String, String), f64> = case["startups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| ((s(&r[0]), s(&r[1])), f(&r[2])))
            .collect();
        let jobs: Vec<(String, String, Option<i64>)> = case["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| (s(&j[0]), s(&j[1]), j[2].as_i64()))
            .collect();
        let mut params = EftParams::default();
        if let Some(p) = case["params"].as_object().filter(|p| !p.is_empty()) {
            params.margin = f(&p["margin"]);
            params.margin_cap = f(&p["margin_cap"]);
            params.slack = f(&p["slack"]);
            params.limit = p["limit"].as_u64().unwrap() as usize;
        }
        let d = earliest_finish_claim(
            &jobs,
            &lanes,
            &case["asking"].as_i64().unwrap(),
            |g, m| {
                rates[&(g.to_owned(), m.to_owned())].map(|(p, l)| RateEstimate::new(p, "t", 0, l))
            },
            |g, m| startups[&(g.to_owned(), m.to_owned())],
            params,
        );
        let got = d.map_or(Value::Null, |d| {
            json!({
                "mine": d.mine,
                "left": d.left.iter().map(|l| json!([l.job, l.to, l.there, l.here, l.starts, l.busy_first])).collect::<Vec<_>>(),
            })
        });
        check(&format!("eft[{i}]"), &got, &case["out"]);
    }
}

// --- queue plan --------------------------------------------------------------------

fn machine_bench_table(world: &Value) -> HashMap<(String, String), MachineBench> {
    world["machine_bench"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                (s(&r[0]), s(&r[1])),
                MachineBench {
                    pages_per_second: r[2].as_f64(),
                    startup_seconds: r[3].as_f64(),
                },
            )
        })
        .collect()
}

#[test]
fn plan_queue_matches_python() {
    for (i, case) in load("plan.json").as_array().unwrap().iter().enumerate() {
        let (_clock, rm) = build_world(&case["world"]);
        let table = machine_bench_table(&case["world"]);
        let pricing = LanePricing::new(&rm, |g: &str, m: &str| {
            table.get(&(g.to_owned(), m.to_owned())).copied()
        });
        let lane_machines: Option<Vec<String>> = case["lane_machines"]
            .as_array()
            .map(|a| a.iter().map(s).collect());
        let refusals: Option<HashMap<String, String>> = case["refusals"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), s(v))).collect());
        let holds: Option<HashMap<String, Option<String>>> = case["holds"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), opt_s(v))).collect());
        let refusal_fn = |g: &str, m: &str| {
            refusals
                .as_ref()
                .and_then(|r| r.get(&format!("{g}|{m}")).cloned())
        };
        let hold_fn = |g: &str| holds.as_ref().and_then(|h| h.get(g).cloned().flatten());
        let running: Vec<Object> = case["running"]
            .as_array()
            .unwrap()
            .iter()
            .map(obj)
            .collect();
        let pending: Vec<Object> = case["pending"]
            .as_array()
            .unwrap()
            .iter()
            .map(obj)
            .collect();
        let inputs = PlanInputs {
            lane_count: case["lane_count"].as_u64().unwrap() as usize,
            lane_machines: lane_machines.as_deref(),
            pricing: &pricing,
            now: f(&case["now"]),
            through: case["through"].as_i64(),
            refusal_for: if refusals.is_some() {
                Some(&refusal_fn)
            } else {
                None
            },
            hold_for: if holds.is_some() {
                Some(&hold_fn)
            } else {
                None
            },
        };
        let plan = plan_queue(&running, &pending, &inputs);
        let got = json!({
            "running": plan.running,
            "pending": plan.pending,
            "done_in": plan.done_in,
            "done_at": plan.done_at,
        });
        check(&format!("plan[{i}]"), &got, &case["out"]);
    }
}

// --- congestion --------------------------------------------------------------------

#[test]
fn congestion_matches_python() {
    let data = load("congestion.json");
    for (i, case) in data["summarize"].as_array().unwrap().iter().enumerate() {
        let got = congestion::summarize(&case["raw"]).map_or(Value::Null, Value::Object);
        check(&format!("summarize[{i}]"), &got, &case["out"]);
    }
    for (i, case) in data["verdict"].as_array().unwrap().iter().enumerate() {
        let summary = obj(&case["summary"]);
        check(
            &format!("verdict[{i}]"),
            &json!(congestion::pipeline_verdict(&summary)),
            &case["verdict"],
        );
        check(
            &format!("widen[{i}]"),
            &json!(congestion::widen_target(&summary)),
            &case["widen"],
        );
    }
    for (i, case) in data["events"].as_array().unwrap().iter().enumerate() {
        let got =
            congestion::summarize_event_stats(&case["stats"]).map_or(Value::Null, Value::Object);
        check(&format!("event[{i}]"), &got, &case["out"]);
    }
    for (i, case) in data["records"].as_array().unwrap().iter().enumerate() {
        let timing = VolumeTiming {
            volume_pages: case["volume_pages"].as_i64(),
            volume_seconds: case["volume_seconds"].as_f64(),
            volume_first: case["volume_first"].as_bool().unwrap(),
        };
        let got = congestion::build_record(
            &obj(&case["summary"]),
            case["volume"].as_str().unwrap(),
            f(&case["at"]),
            timing,
        );
        check(&format!("record[{i}]"), &Value::Object(got), &case["out"]);
    }
    for (i, case) in data["average"].as_array().unwrap().iter().enumerate() {
        let got = congestion::average_runs(case["runs"].as_array().unwrap())
            .map_or(Value::Null, Value::Object);
        check(&format!("average[{i}]"), &got, &case["out"]);
    }
}

// --- small helpers -----------------------------------------------------------------

#[test]
fn misc_matches_python() {
    let data = load("misc.json");
    for (i, case) in data["progress"].as_array().unwrap().iter().enumerate() {
        let a = &case["in"];
        let (p, e, st) = progress_metrics(
            a[0].as_i64().unwrap(),
            a[1].as_i64().unwrap(),
            f(&a[2]),
            a[3].as_f64(),
        );
        check(&format!("progress[{i}]"), &json!([p, e, st]), &case["out"]);
    }
    for (i, case) in data["recheck"].as_array().unwrap().iter().enumerate() {
        let got = recheck_after(case["pending"].as_array().unwrap(), f(&case["now"]));
        check(&format!("recheck[{i}]"), &json!(got), &case["out"]);
    }
    for case in data["iso"].as_array().unwrap() {
        assert_eq!(
            py::iso_utc(f(&case[0])),
            s(&case[1]),
            "iso_utc({})",
            case[0]
        );
    }
    for (i, case) in data["outlook"].as_array().unwrap().iter().enumerate() {
        let owed: Vec<OwedRow> = case["owed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| OwedRow {
                id: s(&r[0]),
                name: s(&r[1]),
                primary: r[2].as_bool().unwrap(),
            })
            .collect();
        let planned: Vec<Object> = case["planned"]
            .as_array()
            .unwrap()
            .iter()
            .map(obj)
            .collect();
        check(
            &format!("outlook[{i}]"),
            &json!(pending_entries(&owed, "S", "V", &planned)),
            &case["out"],
        );
    }
    for case in data["round"].as_array().unwrap() {
        let x = f(&case[0]);
        let n = case[1].as_u64().unwrap() as usize;
        let got = py::round_to(x, n);
        assert!(
            got == f(&case[2]) || (got == 0.0 && f(&case[2]) == 0.0),
            "round({x}, {n}) = {got} vs {}",
            case[2]
        );
        assert_eq!(py::round_int(x), case[3].as_i64().unwrap(), "round({x})");
        assert_eq!(py::fmt_fixed(x, 0), s(&case[4]), "{x}:.0f");
        assert_eq!(py::fmt_fixed(x, 1), s(&case[5]), "{x}:.1f");
        assert_eq!(py::fmt_percent0(x / 100.0), s(&case[6]), "{x}/100:.0%");
        assert_eq!(py::float_repr(x), s(&case[7]), "repr({x})");
    }
    for case in data["sum"].as_array().unwrap() {
        let vals: Vec<f64> = case[0].as_array().unwrap().iter().map(f).collect();
        let want = f(&case[1]);
        let got = py::sum(vals.iter().copied());
        assert!(got == want, "sum({vals:?}) = {got} vs {want}");
    }
    for case in data["median"].as_array().unwrap() {
        let vals: Vec<i64> = case[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(py::median_int(&vals), case[1].as_i64(), "median({vals:?})");
    }
    for (i, case) in data["dumps"].as_array().unwrap().iter().enumerate() {
        assert_eq!(dumps_indent2(&case[0]), s(&case[1]), "dumps[{i}]");
    }
    for case in data["retry"].as_array().unwrap() {
        let got = failures::retry_delay_seconds(f(&case[0]), case[1].as_i64().unwrap());
        assert_eq!(got, f(&case[2]), "retry({}, {})", case[0], case[1]);
    }
    for (i, case) in data["busy"].as_array().unwrap().iter().enumerate() {
        check(
            &format!("busy[{i}]"),
            &json!(busy_reason(&obj(&case[0]))),
            &case[1],
        );
    }
}

#[test]
fn throughput_matches_python() {
    let data = load("throughput.json");
    for (i, case) in data["of"].as_array().unwrap().iter().enumerate() {
        let samples: Vec<Sample> = case["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|smp| Sample::from_values(smp.get(0), smp.get(1), smp.get(2)))
            .collect();
        check(
            &format!("throughput_of[{i}]"),
            &tp(throughput_of(&samples, case["last_at"].as_f64())),
            &case["out"],
        );
    }
    for (i, case) in data["profile"].as_array().unwrap().iter().enumerate() {
        check(
            &format!("profile[{i}]"),
            &tp(profile_throughput(Some(&case["runs"]))),
            &case["out"],
        );
    }
}

// --- worker helpers ------------------------------------------------------------------

#[test]
fn failure_records_match_python() {
    let data = load("worker.json");
    for (i, case) in data["records"].as_array().unwrap().iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let store = FailureStore::new(dir.path());
        for step in case["steps"].as_array().unwrap() {
            let series = s(&step["series"]);
            let volume = s(&step["volume"]);
            let primary = step["gen"] == "g-1";
            let (name, detector) = if primary {
                ("main", None)
            } else {
                ("fast ✓", Some("rtdetr"))
            };
            let rel_cbz = if series.is_empty() {
                format!("{volume}.cbz")
            } else {
                format!("{series}/{volume}.cbz")
            };
            let key = failures::failure_key(&rel_cbz, primary, name);
            let rec_series = failures::record_series(&series);
            let failure = NewFailure {
                series: &rec_series,
                volume: &volume,
                generation: name,
                engine: "hayai",
                detector,
                error: step["error"].as_str(),
                log_file: if step["error"].is_null() {
                    None
                } else {
                    step["log"].as_str()
                },
            };
            store.record(&key, &failure, f(&step["now"])).unwrap();
        }
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert_eq!(text, s(&case["file"]), "records[{i}]");
    }
    for (i, case) in data["prunes"].as_array().unwrap().iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        for file in case["files"].as_array().unwrap() {
            let path = library.join(file.as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        let store = FailureStore::new(dir.path());
        std::fs::write(&store.path, s(&case["input"])).unwrap();
        let names: Vec<String> = case["names"].as_array().unwrap().iter().map(s).collect();
        store
            .prune(&names, |series, volume| {
                failures::archive_exists_in(&library, series, volume)
            })
            .unwrap();
        let got = std::fs::read_to_string(&store.path).ok();
        assert_eq!(got.as_deref(), case["file"].as_str(), "prunes[{i}]");
    }
}

#[test]
fn speed_report_matches_python() {
    let data = load("worker.json");
    for (i, case) in data["speed"].as_array().unwrap().iter().enumerate() {
        let (clock, rm) = build_world(&case["world"]);
        clock.set(f(&case["clock"]));
        let rows: Vec<RowRef> = case["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r[2].as_bool().unwrap())
            .map(|r| RowRef {
                id: s(&r[0]),
                name: s(&r[1]),
            })
            .collect();
        let running: Vec<Object> = case["running"]
            .as_array()
            .unwrap()
            .iter()
            .map(obj)
            .collect();
        let got = speed_report(&rows, &running, &rm, f(&case["within"]));
        check(&format!("speed[{i}]"), &json!(got), &case["out"]);
    }
}

#[test]
fn unicode_sanity() {
    // A few spot checks against Python, independent of the fixtures.
    let set: HashSet<char> = "ß".chars().collect();
    assert!(set.contains(&'ß'));
    assert_eq!(bunko_sched::job_order::py_casefold("ßİǅΣ"), "ssi\u{307}ǆσ");
    assert!(!bunko_sched::job_order::py_isalnum('\u{0345}'));
    assert_eq!(bunko_sched::job_order::py_decimal('٣'), Some(3));
}
