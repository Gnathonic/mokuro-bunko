//! The admin surface of the OCR generation upgrade: `upgrade` in `PUT/GET /api/settings/ocr`
//! and the census endpoint through the router.

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
async fn settings_round_trip() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/settings", Some(&admin), None)
        .await;
    assert_eq!(
        r.json()["ocr"]["upgrade"],
        json!({"enabled": false, "replace": ["mokuro-legacy", "mokuro"]})
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"upgrade": {"enabled": true, "replace": ["mokuro-legacy", "hayai-nova", "hayai-nova"]}})),
        )
        .await;
    assert_eq!(r.status, 200);
    let want = json!({"enabled": true, "replace": ["mokuro-legacy", "hayai-nova"]});
    assert_eq!(r.json()["ocr"]["upgrade"], want);
    let r = h
        .call("GET", "/_admin/api/settings", Some(&admin), None)
        .await;
    assert_eq!(r.json()["ocr"]["upgrade"], want);
    let saved = h.saved_config();
    assert!(saved.ocr.upgrade.enabled);
    assert_eq!(saved.ocr.upgrade.replace, ["mokuro-legacy", "hayai-nova"]);
    // Other keys alone leave it be.
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"poll_interval": 45})),
        )
        .await;
    assert!(r.json()["ocr"].get("upgrade").is_none());
    let r = h
        .call("GET", "/_admin/api/settings", Some(&admin), None)
        .await;
    assert_eq!(r.json()["ocr"]["upgrade"], want);
}

#[tokio::test]
async fn validation_errors() {
    let h = Harness::new();
    let admin = h.admin();
    for (body, needle) in [
        (json!({"upgrade": 3}), "must be an object"),
        (
            json!({"upgrade": {"replace": "mokuro-legacy"}}),
            "must be a list",
        ),
        (json!({"upgrade": {"replace": [1]}}), "list of strings"),
        (json!({"upgrade": {"replace": ["unknown"]}}), "'unknown'"),
        (
            json!({"upgrade": {"replace": ["no-such-engine"]}}),
            "no-such-engine",
        ),
    ] {
        let r = h
            .call(
                "PUT",
                "/_admin/api/settings/ocr",
                Some(&admin),
                Some(body.clone()),
            )
            .await;
        assert_eq!(r.status, 400, "{body}");
        assert!(
            r.json()["error"].as_str().unwrap().contains(needle),
            "{body}"
        );
    }
    assert!(!h.saved_config().ocr.upgrade.enabled);
}

struct Census {
    applied: Mutex<Vec<bool>>,
}

impl OcrAdmin for Census {
    fn runtime_status(&self, c: &Config) -> Value {
        NoOcr.runtime_status(c)
    }
    fn processors(&self, c: &Config) -> Value {
        NoOcr.processors(c)
    }
    fn generations_payload(&self, c: &Config) -> Value {
        NoOcr.generations_payload(c)
    }
    fn generation_stats(&self, c: &Config) -> Value {
        NoOcr.generation_stats(c)
    }
    fn apply(&self, c: &Config) -> Option<Value> {
        self.applied.lock().push(c.ocr.upgrade.enabled);
        Some(json!({"applied": true, "installing": false, "restart_required": false, "reason": ""}))
    }
    fn prune(&self, _ids: &[String]) {}
    fn derive(&self, c: &Config, spec: &Value, p: Option<&str>) -> Result<Value, OcrError> {
        NoOcr.derive(c, spec, p)
    }
    fn set_pools(
        &self,
        _c: &Config,
        _r: &Generation,
        _p: &str,
        _v: &Value,
    ) -> Result<Value, OcrError> {
        Err(OcrError::not_found())
    }
    fn bench(&self, _c: &Config, _k: &str, _r: BenchRequest) -> Result<(u16, Value), OcrError> {
        Err(OcrError::not_found())
    }
    fn refresh_devices(&self) -> Value {
        NoOcr.refresh_devices()
    }
    fn other(
        &self,
        _c: &Config,
        method: &Method,
        path: &[&str],
        _q: &str,
        body: &Value,
    ) -> Option<Result<(u16, Value), OcrError>> {
        match (method.as_str(), path) {
            ("GET", ["ocr", "upgrade"]) => Some(Ok((
                200,
                json!({"enabled": true, "replace": ["mokuro-legacy"], "families": {"mokuro-legacy": 3, "unknown": 1},
                       "ready": 1, "needs_ocr": 1, "skipped_missing_pages": 0, "skipped_edited": 1,
                       "edited_volumes": ["a/Vol 1.cbz"]}),
            ))),
            ("POST", ["ocr", "upgrade", "a", "Vol%201.cbz"]) => {
                Some(Ok((200, json!({"success": true, "mode": body["force"]}))))
            }
            _ => None,
        }
    }
}

#[tokio::test]
async fn census_through_router_and_live_apply() {
    let ocr = Arc::new(Census {
        applied: Mutex::new(vec![]),
    });
    let h = Harness::with(Options {
        ocr: Some(ocr.clone()),
        ..Options::default()
    });
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/ocr/upgrade", Some(&admin), None)
        .await;
    assert_eq!(r.status, 200);
    let c = r.json();
    for k in [
        "enabled",
        "replace",
        "families",
        "ready",
        "needs_ocr",
        "skipped_missing_pages",
        "skipped_edited",
        "edited_volumes",
    ] {
        assert!(c.get(k).is_some(), "{k}");
    }
    let r = h
        .call(
            "POST",
            "/_admin/api/ocr/upgrade/a/Vol%201.cbz",
            Some(&admin),
            Some(json!({"force": true})),
        )
        .await;
    assert_eq!(r.json(), json!({"success": true, "mode": true}));

    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"upgrade": {"enabled": true}})),
        )
        .await;
    assert_eq!(r.json()["applied"], true);
    assert_eq!(*ocr.applied.lock(), [true]);
}

#[tokio::test]
async fn census_is_404_without_ocr() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/ocr/upgrade", Some(&admin), None)
        .await;
    assert_eq!(r.status, 404);
}
