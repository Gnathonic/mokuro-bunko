//! OCR admin endpoints over `NoOcr` (no OCR in this process) and over a recording
//! `OcrAdmin`, to pin the HTTP side's contract with the trait.

mod admin_support;

use admin_support::{Harness, Options};
use bunko_core::Config;
use bunko_core::generations::Generation;
use bunko_server::admin::ocr::{BenchRequest, NoOcr, OcrAdmin, OcrError};
use http::Method;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn no_ocr_generations_payload_and_catalog() {
    let h = Harness::with(Options {
        configure: Some(Box::new(|c: &mut Config| {
            c.ocr.generations = bunko_core::generations::parse_generation_list(&json!([
                {"id": "g-1", "name": "legacy", "engine": "mokuro", "primary": true},
                {"id": "g-2", "name": "nova", "engine": "hayai-nova", "detector": "ctd"},
                {"id": "g-3", "name": "lines", "engine": "ppocr-manga"},
            ]))
            .unwrap()
            .rows;
        })),
        ..Options::default()
    });
    let admin = h.admin();
    let r = h.call("GET", "/_admin/api/ocr/generations", Some(&admin), None).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["stats_pending", "generations", "catalog", "processors", "local_processing", "autobench"]);
    assert_eq!(body["local_processing"], false);
    let engines: Vec<&str> = body["catalog"]["engines"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(engines, ["hayai-nova", "paddle-manga", "ppocr-manga"]);
    assert_eq!(body["catalog"]["detectors"].as_array().unwrap().len(), 1);
    let rows = body["generations"].as_array().unwrap();
    let legacy = rows.iter().find(|r| r["id"] == "g-1").unwrap();
    assert!(legacy["retired"].as_str().unwrap().contains("removed in 0.7"));
    assert_eq!(legacy["enabled"], false);
    let nova = rows.iter().find(|r| r["id"] == "g-2").unwrap();
    assert_eq!(nova["detector"], "ppocr-manga", "a removed detector is migrated");
    assert_eq!(nova["retired"], Value::Null);
    assert_eq!(nova["road"], "reconciled");
    assert_eq!(nova["precision_applies"], true);
    assert!(nova.get("precision_on").is_some());
    let lines = rows.iter().find(|r| r["id"] == "g-3").unwrap();
    assert_eq!(lines["detector_locked"], true);
    assert_eq!(lines["road"], "line");
    assert_eq!(lines["stages"][0]["device_locked_reason"], "PP-OCRv6's CTC recognizer runs on the CPU");
    assert!(lines.get("precision_on").is_none());

    let r = h.call("GET", "/_admin/api/ocr/generations/stats", Some(&admin), None).await.json();
    assert_eq!(r["stats_pending"], false);
    assert_eq!(r["generations"]["g-2"], json!({"volumes_done": null, "volumes_total": null, "volumes_skipped": 0, "volumes_by_machine": {}}));

    let r = h.call("GET", "/_admin/api/processors", Some(&admin), None).await.json();
    assert_eq!(r, json!({"processors": [], "failed_logins": [], "last_disconnect": null, "local_processing": false, "processing_hold": null, "speed": []}));
    let r = h.call("POST", "/_admin/api/ocr/devices/refresh", Some(&admin), None).await.json();
    assert_eq!(r["success"], true);
    assert_eq!(r["devices"][1]["id"], "cpu");
}

#[tokio::test]
async fn no_ocr_bench_derive_and_pools() {
    let h = Harness::new();
    let admin = h.admin();
    // No bench endpoints: the page hides benchmark controls on a 404 for a real row.
    for m in ["GET", "POST", "DELETE"] {
        let r = h.call(m, "/_admin/api/ocr/generations/g-1/bench", Some(&admin), None).await;
        assert_eq!((r.status.as_u16(), r.json()), (404, json!({"error": "API endpoint not found"})), "{m}");
    }
    let r = h.call("PUT", "/_admin/api/ocr/generations/g-1/bench", Some(&admin), Some(json!({}))).await;
    assert_eq!(r.status, 404);

    let r = h.call("POST", "/_admin/api/ocr/generations/derive", Some(&admin), Some(json!({"spec": {"engine": "paddle-manga", "detector": "ppocr-manga"}}))).await;
    let d = r.json();
    assert_eq!(d["road"], "reconciled");
    let keys: Vec<&str> = d["stages"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["detect", "engine", "post"]);
    assert_eq!(d["stages"][1]["name"], "engine read + reconcile");
    let r = h.call("POST", "/_admin/api/ocr/generations/derive", Some(&admin), Some(json!({"spec": {"engine": "mokuro"}}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["field"], "engine");
    assert!(r.json().as_object().unwrap().contains_key("row"));
    let r = h.call("POST", "/_admin/api/ocr/generations/derive", Some(&admin), Some(json!({"spec": {}, "processor": "gpu-box"}))).await;
    assert_eq!(r.json(), json!({"error": "no processor called 'gpu-box' is known"}));

    let r = h.call("PUT", "/_admin/api/ocr/generations/g-1/pools", Some(&admin), Some(json!({"processor": "local"}))).await;
    assert_eq!(r.json(), json!({"error": "processor must name a processor"}));
    let r = h.call("PUT", "/_admin/api/ocr/generations/nope/pools", Some(&admin), Some(json!({"processor": "gpu-box"}))).await;
    assert_eq!(r.json(), json!({"error": "there is no generation 'nope'"}));
    let r = h.call("PUT", "/_admin/api/ocr/generations/g-1/pools", Some(&admin), Some(json!({"processor": "gpu-box", "pools": {}}))).await;
    assert_eq!(r.json(), json!({"error": "no processor called 'gpu-box' is known"}));
    let r = h.call("GET", "/_admin/api/ocr/holds", Some(&admin), None).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn generations_put_validates_saves_and_round_trips() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(json!({}))).await;
    assert_eq!(r.json(), json!({"error": "generations is required (a list of rows, in run order)", "row": null, "field": null}));
    let r = h.raw("PUT", "/_admin/api/ocr/generations", Some(&admin), b"nope".to_vec()).await;
    assert_eq!(r.json(), json!({"error": "Invalid JSON body", "row": null, "field": null}));
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(json!({"generations": [{"id": "g-9", "name": "x", "engine": "hayai-nova"}]}))).await;
    assert_eq!(
        r.json(),
        json!({"error": "ocr.generations[0]: id 'g-9' is not a generation this server knows; leave id out for a new row", "row": 0, "field": "id"})
    );
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(json!({"generations": [{"name": "x", "engine": "nope"}]}))).await;
    assert_eq!(r.status, 400);
    assert_eq!((r.json()["row"].clone(), r.json()["field"].clone()), (json!(0), json!("engine")));

    let rows = json!({"generations": [
        {"id": "g-1", "name": "nova", "primary": true, "enabled": true, "engine": "hayai-nova", "detector": "ppocr-manga", "patch_budget": 384, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}},
        {"name": "vl", "primary": false, "enabled": true, "engine": "paddle-manga", "detector": "ppocr-manga", "patch_budget": null, "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}},
    ]});
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(rows)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let body = r.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys[0], "success");
    for k in ["generations", "catalog", "applied", "installing", "restart_required", "reason", "ocr_runtime"] {
        assert!(body.get(k).is_some(), "{k}");
    }
    assert_eq!(body["generations"].as_array().unwrap().len(), 2);
    assert_eq!(body["generations"][1]["sidecar"], "<Volume>.vl.mokuro");
    let saved = h.saved_config();
    assert_eq!(saved.ocr.generations.len(), 2);
    assert_eq!(saved.ocr.generations[0].patch_budget, 384);
    assert_eq!(saved.ocr.generations[1].name, "vl");
    let listed = h.call("GET", "/_admin/api/ocr/generations", Some(&admin), None).await.json();
    assert_eq!(listed["generations"], body["generations"]);
}

/// Records what the HTTP side asks of the trait; delegates the rest to `NoOcr`.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl OcrAdmin for Recorder {
    fn runtime_status(&self, c: &Config) -> Value {
        NoOcr.runtime_status(c)
    }
    fn processors(&self, c: &Config) -> Value {
        NoOcr.processors(c)
    }
    fn generations_payload(&self, c: &Config) -> Value {
        NoOcr.generations_payload(c)
    }
    fn generation_stats(&self, _c: &Config) -> Value {
        json!({"stats_pending": true})
    }
    fn apply(&self, c: &Config) -> Option<Value> {
        self.calls.lock().push(format!("apply poll={}", c.ocr.poll_interval));
        Some(json!({"applied": true, "installing": false, "restart_required": false, "reason": "applied live"}))
    }
    fn prune(&self, ids: &[String]) {
        self.calls.lock().push(format!("prune {}", ids.join(",")));
    }
    fn derive(&self, c: &Config, spec: &Value, p: Option<&str>) -> Result<Value, OcrError> {
        NoOcr.derive(c, spec, p)
    }
    fn set_pools(&self, _c: &Config, row: &Generation, processor: &str, pools: &Value) -> Result<Value, OcrError> {
        self.calls.lock().push(format!("pools {} {processor}", row.id));
        Ok(json!({"success": true, "pools": pools}))
    }
    fn bench(&self, _c: &Config, key: &str, request: BenchRequest) -> Result<(u16, Value), OcrError> {
        self.calls.lock().push(format!("bench {key} {request:?}"));
        match request {
            BenchRequest::Enqueue { .. } => Ok((202, json!({"state": "queued"}))),
            BenchRequest::Get { .. } => Ok((200, json!({"state": "idle"}))),
            BenchRequest::Cancel { .. } => Err(OcrError::at(400, "nothing is queued", None, None)),
        }
    }
    fn refresh_devices(&self) -> Value {
        NoOcr.refresh_devices()
    }
    fn queue_changed(&self) {
        self.calls.lock().push("queue".into());
    }
    fn other(&self, _c: &Config, method: &Method, path: &[&str], query: &str, _body: &Value) -> Option<Result<(u16, Value), OcrError>> {
        (path == ["ocr", "holds"] && method == Method::GET).then(|| Ok((200, json!({"holds": [], "query": query}))))
    }
}

#[tokio::test]
async fn the_http_side_drives_the_trait() {
    let rec = Arc::new(Recorder::default());
    let h = Harness::with(Options { ocr: Some(rec.clone()), ..Options::default() });
    let admin = h.admin();

    let r = h.call("PUT", "/_admin/api/settings/ocr", Some(&admin), Some(json!({"poll_interval": 30}))).await.json();
    assert_eq!(r["applied"], false, "unchanged: nothing to apply");
    assert!(rec.calls.lock().is_empty());
    let r = h.call("PUT", "/_admin/api/settings/ocr", Some(&admin), Some(json!({"poll_interval": 12}))).await.json();
    assert_eq!((r["applied"].clone(), r["reason"].clone()), (json!(true), json!("applied live")));
    assert_eq!(rec.calls.lock().last().unwrap(), "apply poll=12");

    let rows = json!({"generations": [{"id": "g-1", "name": "renamed", "primary": true, "enabled": true, "engine": "hayai-nova", "detector": "ppocr-manga", "patch_budget": 512, "pools": {}}]});
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(rows.clone())).await.json();
    assert_eq!(r["applied"], true);
    assert_eq!(&rec.calls.lock()[1..], ["apply poll=12", "prune g-1"]);
    let before = rec.calls.lock().len();
    let r = h.call("PUT", "/_admin/api/ocr/generations", Some(&admin), Some(rows)).await.json();
    assert_eq!((r["applied"].clone(), r["restart_required"].clone()), (json!(false), json!(false)), "unchanged list");
    assert_eq!(rec.calls.lock().len(), before);

    h.call("PUT", "/_admin/api/settings/queue", Some(&admin), Some(json!({"display": "minimal"}))).await;
    assert_eq!(rec.calls.lock().last().unwrap(), "queue");

    let r = h.call("POST", "/_admin/api/ocr/generations/draft-a1/bench", Some(&admin), Some(json!({"spec": {"engine": "hayai-nova"}, "pages": 8}))).await;
    assert_eq!((r.status.as_u16(), r.json()), (202, json!({"state": "queued"})));
    assert!(rec.calls.lock().last().unwrap().contains("processor: \"local\""));
    let r = h.call("GET", "/_admin/api/ocr/generations/g-1/bench?processor=gpu-box", Some(&admin), None).await;
    assert_eq!(r.json(), json!({"state": "idle"}));
    assert!(rec.calls.lock().last().unwrap().contains("Get { processor: Some(\"gpu-box\") }"));
    let r = h.call("DELETE", "/_admin/api/ocr/generations/g-1/bench", Some(&admin), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, json!({"error": "nothing is queued", "row": null, "field": null})));

    let r = h.call("PUT", "/_admin/api/ocr/generations/g-1/pools", Some(&admin), Some(json!({"processor": "gpu-box", "pools": {"stage_workers": {"detect": 2}}}))).await;
    assert_eq!(r.json()["pools"]["stage_workers"]["detect"], 2);
    assert_eq!(rec.calls.lock().last().unwrap(), "pools g-1 gpu-box");

    let r = h.call("GET", "/_admin/api/ocr/holds?x=1", Some(&admin), None).await;
    assert_eq!(r.json(), json!({"holds": [], "query": "x=1"}));
    let r = h.call("GET", "/_admin/api/ocr/generations/stats", Some(&admin), None).await;
    assert_eq!(r.json(), json!({"stats_pending": true}));
}
