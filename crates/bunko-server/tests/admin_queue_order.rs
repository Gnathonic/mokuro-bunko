//! `queue_order` in `GET /api/settings` and `PUT /api/settings/ocr`: how the OCR queue
//! takes work (by generation or by volume), its MEGA default and the live apply.

mod admin_support;

use admin_support::{Harness, Options};
use bunko_core::Config;
use bunko_core::config::{LibraryBackend, QueueOrder};
use bunko_core::generations::Generation;
use bunko_server::admin::ocr::{BenchRequest, NoOcr, OcrAdmin, OcrError};
use http::Method;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

/// Records the order each live apply was given.
struct Applies(Mutex<Vec<QueueOrder>>);

impl OcrAdmin for Applies {
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
        self.0.lock().push(c.queue_order());
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
        _method: &Method,
        _path: &[&str],
        _q: &str,
        _body: &Value,
    ) -> Option<Result<(u16, Value), OcrError>> {
        None
    }
}

/// The admin harness over a library on `backend`, with an admin's token.
struct Admin {
    h: Harness,
    token: String,
}

fn harness(backend: LibraryBackend) -> (Admin, Arc<Applies>) {
    let applies = Arc::new(Applies(Mutex::new(Vec::new())));
    let h = Harness::with(Options {
        ocr: Some(applies.clone()),
        configure: Some(Box::new(move |c: &mut Config| {
            c.storage.library.backend = backend;
        })),
        ..Options::default()
    });
    let token = h.admin();
    (Admin { h, token }, applies)
}

impl Admin {
    fn saved_config(&self) -> Config {
        self.h.saved_config()
    }
}

async fn put(a: &Admin, body: Value) -> admin_support::Reply {
    a.h.call(
        "PUT",
        "/_admin/api/settings/ocr",
        Some(&a.token),
        Some(body),
    )
    .await
}

async fn shown(a: &Admin) -> (Value, Value) {
    let r =
        a.h.call("GET", "/_admin/api/settings", Some(&a.token), None)
            .await;
    assert_eq!(r.status, 200);
    let ocr = r.json()["ocr"].clone();
    (
        ocr["queue_order"].clone(),
        ocr["queue_order_effective"].clone(),
    )
}

#[tokio::test]
async fn get_shows_the_configured_and_effective_order() {
    let (h, _) = harness(LibraryBackend::Local);
    assert_eq!(shown(&h).await, (json!("auto"), json!("generation")));
    let (mega, _) = harness(LibraryBackend::Mega);
    assert_eq!(
        shown(&mega).await,
        (json!("auto"), json!("volume")),
        "a MEGA library takes work by volume unless set"
    );
}

#[tokio::test]
async fn put_saves_applies_and_echoes_only_when_sent() {
    let (h, applies) = harness(LibraryBackend::Local);
    let r = put(&h, json!({"queue_order": "volume"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["ocr"]["queue_order"], "volume");
    assert_eq!(r.json()["ocr"]["queue_order_effective"], "volume");
    assert_eq!(r.json()["applied"], true);
    assert_eq!(*applies.0.lock(), [QueueOrder::Volume]);
    assert_eq!(h.saved_config().ocr.queue_order, Some(QueueOrder::Volume));
    assert_eq!(shown(&h).await, (json!("volume"), json!("volume")));
    // Other keys alone neither echo it nor change it.
    let r = put(&h, json!({"poll_interval": 45})).await;
    assert_eq!(r.status, 200);
    assert!(r.json()["ocr"].get("queue_order").is_none());
    assert_eq!(h.saved_config().ocr.queue_order, Some(QueueOrder::Volume));
}

#[tokio::test]
async fn put_auto_clears_the_key() {
    let (h, applies) = harness(LibraryBackend::Mega);
    let r = put(&h, json!({"queue_order": "generation"})).await;
    assert_eq!(r.json()["ocr"]["queue_order_effective"], "generation");
    assert_eq!(
        h.saved_config().ocr.queue_order,
        Some(QueueOrder::Generation)
    );
    for auto in [json!("auto"), Value::Null] {
        let r = put(&h, json!({"queue_order": auto})).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["ocr"]["queue_order"], "auto");
        assert_eq!(r.json()["ocr"]["queue_order_effective"], "volume");
        assert_eq!(h.saved_config().ocr.queue_order, None);
    }
    assert_eq!(
        *applies.0.lock(),
        [QueueOrder::Generation, QueueOrder::Volume],
        "applied when the order changed, not when it stayed"
    );
}

#[tokio::test]
async fn put_rejects_other_values() {
    let (h, applies) = harness(LibraryBackend::Local);
    for bad in [
        json!("series"),
        json!(1),
        json!(true),
        json!({"order": "volume"}),
    ] {
        let r = put(&h, json!({"queue_order": bad.clone()})).await;
        assert_eq!(r.status, 400, "{bad}");
        assert_eq!(
            r.json()["error"],
            "queue_order must be auto, generation or volume"
        );
    }
    assert_eq!(h.saved_config().ocr.queue_order, None);
    assert!(applies.0.lock().is_empty());
}
