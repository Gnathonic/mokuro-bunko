//! The cloud queue document (spec §9.1, D10): the pure transform of the server's
//! `build_document` output.

use bunko_remote::status::{
    DONE_LIMIT, DoneEntry, DoneLayer, QUEUE_DOC_MAX_BYTES, cloud_queue_doc, format_iso_s,
    parse_iso_ms, queue_doc_changed,
};
use serde_json::{Value, json};

const NOW: &str = "2026-10-12T10:00:00Z";
const HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    parse_iso_ms(NOW).unwrap()
}

fn job(state: &str, progress: Value) -> Value {
    json!({"kind": "ocr", "id": "hayai", "state": state, "eta": null, "progress": progress})
}

fn volume(series: &str, vol: &str, jobs: Vec<Value>) -> Value {
    json!({
        "series": series,
        "volume": vol,
        "path": format!("/mokuro-reader/{series}/{vol}.cbz"),
        "manifest": "",
        "jobs": jobs,
    })
}

/// What `build_document` produces.
fn server_doc(volumes: Vec<Value>, pending: usize, generated_at: &str) -> Value {
    json!({
        "version": 1,
        "generated_at": generated_at,
        "held": null,
        "next_check_after": 120,
        "pending_volumes": pending,
        "volumes": volumes,
    })
}

fn done(series: &str, vol: &str, at_ms: i64) -> DoneEntry {
    DoneEntry {
        series: series.into(),
        volume: vol.into(),
        at_ms,
        ocr: Some(format!("{series}/{vol}.mokuro")),
        layers: vec![DoneLayer {
            id: "hayai".into(),
            path: format!("{series}/{vol}.hayai.mokuro"),
        }],
    }
}

fn len(v: &Value) -> usize {
    serde_json::to_vec(v).unwrap().len()
}

#[test]
fn adds_instance_truncated_done_in_spec_order() {
    let doc = server_doc(
        vec![
            volume("Kingdom", "Kingdom 13", vec![job("running", json!(0.437))]),
            volume("Kingdom", "Kingdom 14", vec![job("queued", Value::Null)]),
        ],
        1,
        NOW,
    );
    let out = cloud_queue_doc(
        &doc,
        "6f1c",
        &[done("Kingdom", "Kingdom 12", now_ms() - 60_000)],
        QUEUE_DOC_MAX_BYTES,
    );
    let keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "version",
            "generated_at",
            "instance_id",
            "held",
            "next_check_after",
            "pending_volumes",
            "truncated",
            "volumes",
            "done"
        ]
    );
    assert_eq!(out["instance_id"], "6f1c");
    assert_eq!(out["generated_at"], NOW);
    assert_eq!(out["truncated"], false);
    assert_eq!(out["volumes"].as_array().unwrap().len(), 2);
    assert_eq!(
        out["done"][0],
        json!({"series": "Kingdom", "volume": "Kingdom 12", "at": "2026-10-12T09:59:00Z",
               "ocr": "Kingdom/Kingdom 12.mokuro",
               "layers": [{"id": "hayai", "path": "Kingdom/Kingdom 12.hayai.mokuro"}]})
    );
}

#[test]
fn progress_is_quantised_to_a_tenth() {
    let doc = server_doc(
        vec![volume(
            "S",
            "V",
            vec![
                job("running", json!(0.437)),
                job("running", json!(0.66)),
                job("running", json!(0.0)),
                job("running", json!(1.0)),
                job("queued", Value::Null),
            ],
        )],
        0,
        NOW,
    );
    let out = cloud_queue_doc(&doc, "i", &[], QUEUE_DOC_MAX_BYTES);
    let got: Vec<Value> = out["volumes"][0]["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["progress"].clone())
        .collect();
    assert_eq!(
        got,
        vec![json!(0.4), json!(0.7), json!(0.0), json!(1.0), Value::Null]
    );
    let text = serde_json::to_string(&out).unwrap();
    assert!(text.contains("\"progress\":0.4}"), "{text}");
}

#[test]
fn truncated_when_fewer_waiting_listed_than_pending() {
    let waiting: Vec<Value> = (0..100)
        .map(|i| volume("S", &format!("V{i}"), vec![job("queued", Value::Null)]))
        .collect();
    let mut vols = vec![volume("S", "Run", vec![job("running", json!(0.5))])];
    vols.extend(waiting);
    let out = cloud_queue_doc(&server_doc(vols, 837, NOW), "i", &[], QUEUE_DOC_MAX_BYTES);
    assert_eq!(out["truncated"], true);
    assert_eq!(out["pending_volumes"], 837);
    assert_eq!(out["volumes"].as_array().unwrap().len(), 101);

    // More than 100 waiting in the input: capped at 100.
    let many: Vec<Value> = (0..150)
        .map(|i| volume("S", &format!("V{i}"), vec![job("queued", Value::Null)]))
        .collect();
    let out = cloud_queue_doc(&server_doc(many, 150, NOW), "i", &[], usize::MAX);
    assert_eq!(out["volumes"].as_array().unwrap().len(), 100);
    assert_eq!(out["truncated"], true);
}

#[test]
fn trims_waiting_first_to_fit_64_kib() {
    let long = "長".repeat(300); // ~900 bytes of UTF-8 per name
    let mut vols: Vec<Value> = (0..3)
        .map(|i| {
            volume(
                &format!("Run{i}"),
                &format!("{long}{i}"),
                vec![job("running", json!(0.25))],
            )
        })
        .collect();
    vols.extend((0..100).map(|i| {
        volume(
            "Waiting",
            &format!("{long}{i}"),
            vec![job("queued", Value::Null)],
        )
    }));
    let entries: Vec<DoneEntry> = (0..20)
        .map(|i| done("Done", &format!("D{i}"), now_ms() - i * 1000))
        .collect();
    let doc = server_doc(vols, 100, NOW);
    assert!(
        len(&doc) > 2 * QUEUE_DOC_MAX_BYTES,
        "the input must overflow"
    );

    let out = cloud_queue_doc(&doc, "i", &entries, QUEUE_DOC_MAX_BYTES);
    assert!(len(&out) <= QUEUE_DOC_MAX_BYTES, "{}", len(&out));
    let listed = out["volumes"].as_array().unwrap();
    // Every running volume stays, waiting ones are cut from the end, done[] stays.
    assert!(
        listed[..3]
            .iter()
            .all(|v| v["jobs"][0]["state"] == "running")
    );
    let waiting: Vec<&str> = listed[3..]
        .iter()
        .map(|v| v["volume"].as_str().unwrap())
        .collect();
    assert!(!waiting.is_empty() && waiting.len() < 100);
    for (i, w) in waiting.iter().enumerate() {
        assert_eq!(
            *w,
            format!("{long}{i}"),
            "the first waiting volumes are kept"
        );
    }
    assert_eq!(out["truncated"], true);
    assert_eq!(out["done"].as_array().unwrap().len(), 20);
    // Close to the cap, not trimmed far below it.
    assert!(len(&out) > QUEUE_DOC_MAX_BYTES - 2000, "{}", len(&out));

    // A tiny cap empties waiting, then done (oldest first), then running.
    let out = cloud_queue_doc(&doc, "i", &entries, 4000);
    assert!(len(&out) <= 4000, "{}", len(&out));
    assert!(
        out["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["jobs"][0]["state"] == "running")
    );
    assert!(out["done"].as_array().unwrap().is_empty());
}

#[test]
fn done_is_newest_first_last_100_within_24_hours() {
    let now = now_ms();
    let mut entries: Vec<DoneEntry> = (0..150)
        .map(|i| done("S", &format!("V{i}"), now - i * 60_000))
        .collect();
    entries.push(done("S", "Old", now - 25 * HOUR_MS));
    entries.reverse(); // oldest first in, newest first out
    let out = cloud_queue_doc(
        &server_doc(vec![], 0, NOW),
        "i",
        &entries,
        QUEUE_DOC_MAX_BYTES,
    );
    let got: Vec<&str> = out["done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["volume"].as_str().unwrap())
        .collect();
    assert_eq!(got.len(), DONE_LIMIT);
    let want: Vec<String> = (0..100).map(|i| format!("V{i}")).collect();
    assert_eq!(got, want.iter().map(String::as_str).collect::<Vec<_>>());

    // Fewer than 100 in the window: only those.
    let few = vec![
        done("S", "Fresh", now - HOUR_MS),
        done("S", "Edge", now - 24 * HOUR_MS + 1000),
        done("S", "Stale", now - 24 * HOUR_MS - 1000),
    ];
    let out = cloud_queue_doc(&server_doc(vec![], 0, NOW), "i", &few, QUEUE_DOC_MAX_BYTES);
    let got: Vec<&str> = out["done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["volume"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["Fresh", "Edge"]);
    assert_eq!(
        out["done"][0]["at"],
        json!(format_iso_s(now - HOUR_MS)),
        "at is ISO-8601"
    );
}

#[test]
fn only_generated_at_changing_is_not_a_change() {
    let vols = |p: f64| vec![volume("S", "V", vec![job("running", json!(p))])];
    let a = cloud_queue_doc(
        &server_doc(vols(0.41), 0, NOW),
        "i",
        &[],
        QUEUE_DOC_MAX_BYTES,
    );
    let later = "2026-10-12T10:00:30Z";
    let b = cloud_queue_doc(
        &server_doc(vols(0.41), 0, later),
        "i",
        &[],
        QUEUE_DOC_MAX_BYTES,
    );
    assert_ne!(a, b);
    assert!(!queue_doc_changed(&a, &b));
    // Per-page progress inside one tenth: no change either.
    let c = cloud_queue_doc(
        &server_doc(vols(0.44), 0, later),
        "i",
        &[],
        QUEUE_DOC_MAX_BYTES,
    );
    assert!(!queue_doc_changed(&a, &c));
    // Crossing a tenth is a change.
    let d = cloud_queue_doc(
        &server_doc(vols(0.46), 0, later),
        "i",
        &[],
        QUEUE_DOC_MAX_BYTES,
    );
    assert!(queue_doc_changed(&a, &d));
    // So is a new completion.
    let e = cloud_queue_doc(
        &server_doc(vols(0.41), 0, later),
        "i",
        &[done("S", "W", now_ms())],
        QUEUE_DOC_MAX_BYTES,
    );
    assert!(queue_doc_changed(&a, &e));
}
