//! "This server" (`/_admin/api/machine/*`) over a fake [`Machine`]: admin only, the
//! OCR settings saved and handed to the machine, a short cooldown on long actions, and
//! only the fixed job kinds.

mod admin_support;

use admin_support::{Harness, Options};
use bunko_core::{Config, Role};
use bunko_server::machine::Machine;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Default)]
struct Fake {
    locked: Option<String>,
    changed: Mutex<Vec<(String, bool, bool)>>,
    installs: Mutex<Vec<bool>>,
    /// What `remove_refused` says.
    refuse: Option<String>,
    /// Every step of a removal, in order, shared with [`Ocr`].
    steps: Arc<Mutex<Vec<String>>>,
}

impl Machine for Fake {
    fn overview(&self) -> Value {
        json!({"ocr_build": true, "hardware": {"amd_gfx": ["gfx1201"]}})
    }
    fn setup_options(&self) -> Value {
        Value::Null
    }
    fn backend_locked(&self) -> Option<String> {
        self.locked.clone()
    }
    fn ocr_changed(&self, config: &Config, backend_changed: bool, from_setup: bool) -> Value {
        self.changed
            .lock()
            .push((config.ocr.backend.clone(), backend_changed, from_setup));
        json!({"installing": true, "restarting": false, "message": "Installing in the background."})
    }
    fn install(&self, reinstall: bool) -> Result<Value, String> {
        self.installs.lock().push(reinstall);
        Ok(json!({"installing": true}))
    }
    fn remove_refused(&self) -> Option<String> {
        self.refuse.clone()
    }
    fn remove(&self) -> Result<Value, String> {
        self.steps.lock().push("remove".into());
        Ok(
            json!({"removed": ["torch-rocm7.1-2.13.0"], "deferred": [], "failed": [], "freed": 5, "message": ""}),
        )
    }
    fn start_job(&self, kind: &str, _engine: Option<&str>) -> Result<Value, String> {
        Ok(json!({"id": 1, "kind": kind, "state": "running", "output": [], "next": 0}))
    }
    fn job(&self, id: u64, _from: u64) -> Option<Value> {
        (id == 1).then(|| json!({"id": 1, "state": "ok", "output": ["fine"], "next": 1}))
    }
    fn logs(&self, lines: usize) -> Value {
        json!({"path": "/x/server.log", "text": format!("{lines} lines")})
    }
}

/// This server's OCR as the admin panel drives it: the settings it is handed, and when
/// it is asked to have stopped.
struct Ocr {
    steps: Arc<Mutex<Vec<String>>>,
}

impl bunko_server::admin::OcrAdmin for Ocr {
    fn runtime_status(&self, config: &Config) -> Value {
        bunko_server::admin::NoOcr.runtime_status(config)
    }
    fn processors(&self, config: &Config) -> Value {
        bunko_server::admin::NoOcr.processors(config)
    }
    fn generations_payload(&self, config: &Config) -> Value {
        bunko_server::admin::NoOcr.generations_payload(config)
    }
    fn generation_stats(&self, config: &Config) -> Value {
        bunko_server::admin::NoOcr.generation_stats(config)
    }
    fn apply(&self, config: &Config) -> Option<Value> {
        self.steps
            .lock()
            .push(format!("apply local={}", config.ocr.local_processing));
        Some(
            json!({"applied": true, "installing": false, "restart_required": false, "reason": "", "returned": 2}),
        )
    }
    fn wait_local_stopped(&self) {
        self.steps.lock().push("stopped".into());
    }
    fn derive(
        &self,
        config: &Config,
        spec: &Value,
        processor: Option<&str>,
    ) -> Result<Value, bunko_server::admin::ocr::OcrError> {
        bunko_server::admin::NoOcr.derive(config, spec, processor)
    }
    fn set_pools(
        &self,
        config: &Config,
        row: &bunko_core::generations::Generation,
        processor: &str,
        pools: &Value,
    ) -> Result<Value, bunko_server::admin::ocr::OcrError> {
        bunko_server::admin::NoOcr.set_pools(config, row, processor, pools)
    }
    fn bench(
        &self,
        config: &Config,
        key: &str,
        request: bunko_server::admin::ocr::BenchRequest,
    ) -> Result<(u16, Value), bunko_server::admin::ocr::OcrError> {
        bunko_server::admin::NoOcr.bench(config, key, request)
    }
    fn refresh_devices(&self) -> Value {
        bunko_server::admin::NoOcr.refresh_devices()
    }
}

fn harness_with_ocr(fake: Arc<Fake>) -> Harness {
    Harness::with(Options {
        machine: Some(fake.clone()),
        ocr: Some(Arc::new(Ocr {
            steps: fake.steps.clone(),
        })),
        ..Options::default()
    })
}

fn harness(fake: Arc<Fake>) -> Harness {
    Harness::with(Options {
        machine: Some(fake),
        ..Options::default()
    })
}

#[tokio::test]
async fn admins_only_and_absent_without_a_machine() {
    let h = harness(Arc::new(Fake::default()));
    let user = h.login("reader", Role::Registered);
    let r = h
        .call("GET", "/_admin/api/machine", Some(&user), None)
        .await;
    assert_eq!(r.status, 403);
    let r = h.call("GET", "/_admin/api/machine", None, None).await;
    assert!(r.status == 401 || r.status == 403, "{}", r.status);
    // Every route, for everyone but an admin: refused, nothing about the machine.
    let inviter = h.login("inv", Role::Inviter);
    let routes = [
        ("GET", "/_admin/api/machine", None),
        (
            "PUT",
            "/_admin/api/machine/ocr",
            Some(json!({"backend": "cpu"})),
        ),
        ("POST", "/_admin/api/machine/install", Some(json!({}))),
        ("POST", "/_admin/api/machine/remove", Some(json!({}))),
        (
            "POST",
            "/_admin/api/machine/jobs",
            Some(json!({"kind": "doctor"})),
        ),
        ("GET", "/_admin/api/machine/jobs/1", None),
        ("GET", "/_admin/api/machine/logs", None),
    ];
    for (method, path, body) in &routes {
        for token in [None, Some(user.as_str()), Some(inviter.as_str())] {
            let r = h.call(method, path, token, body.clone()).await;
            assert!(
                r.status == 401 || r.status == 403,
                "{method} {path} as {token:?}: {}",
                r.status
            );
            let text = String::from_utf8_lossy(&r.bytes);
            assert!(
                !text.contains("gfx1201") && !text.contains("server.log"),
                "{text}"
            );
        }
    }
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/machine", Some(&admin), None)
        .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["hardware"]["amd_gfx"][0], "gfx1201");
    assert_eq!(v["config"]["backend"], "auto");

    let none = Harness::with(Options::default());
    let admin = none.admin();
    let r = none
        .call("GET", "/_admin/api/machine", Some(&admin), None)
        .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn the_backend_is_saved_then_applied() {
    let fake = Arc::new(Fake::default());
    let h = harness(fake.clone());
    let admin = h.admin();
    let r = h
        .call(
            "PUT",
            "/_admin/api/machine/ocr",
            Some(&admin),
            Some(json!({"backend": "rocm"})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.bytes));
    assert_eq!(r.json()["config"]["backend"], "rocm");
    assert_eq!(r.json()["result"]["installing"], true);
    assert_eq!(h.saved_config().ocr.backend, "rocm");
    assert_eq!(
        fake.changed.lock().as_slice(),
        &[("rocm".to_string(), true, false)]
    );
    // Not a backend a page may pick; and too soon after the last change.
    let r = h
        .call(
            "PUT",
            "/_admin/api/machine/ocr",
            Some(&admin),
            Some(json!({"backend": "skip"})),
        )
        .await;
    assert_eq!(r.status, 400);
    let r = h
        .call(
            "PUT",
            "/_admin/api/machine/ocr",
            Some(&admin),
            Some(json!({"backend": "cpu"})),
        )
        .await;
    assert_eq!(r.status, 429);
    assert_eq!(h.saved_config().ocr.backend, "rocm");
}

#[tokio::test]
async fn a_backend_the_environment_sets_is_not_changed() {
    let fake = Arc::new(Fake {
        locked: Some("The OCR backend is set by MOKURO_OCR_BACKEND (cpu)".into()),
        ..Fake::default()
    });
    let h = harness(fake.clone());
    let admin = h.admin();
    let r = h
        .call(
            "PUT",
            "/_admin/api/machine/ocr",
            Some(&admin),
            Some(json!({"backend": "rocm"})),
        )
        .await;
    assert_eq!(r.status, 409);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .contains("MOKURO_OCR_BACKEND")
    );
    assert_eq!(h.saved_config().ocr.backend, "auto");
    assert!(fake.changed.lock().is_empty());
}

#[tokio::test]
async fn install_jobs_and_logs() {
    let fake = Arc::new(Fake::default());
    let h = harness(fake.clone());
    let admin = h.admin();
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/install",
            Some(&admin),
            Some(json!({"reinstall": true})),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(fake.installs.lock().as_slice(), &[true]);
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/install",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 429);
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/jobs",
            Some(&admin),
            Some(json!({"kind": "doctor"})),
        )
        .await;
    assert_eq!(r.status, 200);
    let r = h
        .call(
            "GET",
            "/_admin/api/machine/jobs/1?from=0",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json()["output"][0], "fine");
    let r = h
        .call("GET", "/_admin/api/machine/jobs/9", Some(&admin), None)
        .await;
    assert_eq!(r.status, 404);
    let r = h
        .call(
            "GET",
            "/_admin/api/machine/logs?lines=99999",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json()["text"], "2000 lines");
}

/// A write from another web page is refused like every admin write (CSRF).
#[tokio::test]
async fn a_cross_site_write_is_refused() {
    let h = harness(Arc::new(Fake::default()));
    let admin = h.admin();
    let req = http::Request::builder()
        .method("POST")
        .uri("/_admin/api/machine/install")
        .header("authorization", format!("Bearer {admin}"))
        .header("content-type", "text/plain")
        .header("origin", "https://evil.example")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, 403, "{}", String::from_utf8_lossy(&r.bytes));
}

/// Regression (owner report: "I changed settings to remove this machine's OCR backend,
/// but it still keeps processing OCR tasks"). Remove deleted the packs and left OCR on
/// this machine on: the processor went on from the libraries still loaded, and nothing
/// stopped an install from putting a pack back. Removing the OCR backend now turns OCR
/// on this machine off first (saved, and applied as its toggle does), waits for it to
/// have stopped, and only then removes the packs; the answer says so.
#[tokio::test]
async fn remove_turns_ocr_on_this_machine_off_first() {
    let fake = Arc::new(Fake::default());
    let h = harness_with_ocr(fake.clone());
    let admin = h.admin();
    assert!(h.saved_config().ocr.local_processing || h.core.config.read().ocr.local_processing);
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/remove",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(
        fake.steps.lock().as_slice(),
        ["apply local=false", "stopped", "remove"],
        "off, stopped, then removed"
    );
    assert!(!h.saved_config().ocr.local_processing, "saved off");
    assert!(
        !h.core.config.read().ocr.local_processing,
        "off in the running server"
    );
    assert_eq!(v["local_turned_off"], true);
    assert_eq!(v["returned"], 2);
    assert_eq!(v["config"]["local_processing"], false);
    assert_eq!(v["removed"][0], "torch-rocm7.1-2.13.0");

    // The page shows the toggle off afterwards.
    let r = h
        .call("GET", "/_admin/api/machine", Some(&admin), None)
        .await;
    assert_eq!(r.json()["config"]["local_processing"], false);
}

/// Removed again while OCR on this machine is off: nothing to turn off, but a stop still
/// under way is waited for before the files go.
#[tokio::test]
async fn remove_with_ocr_already_off_still_waits_for_it_to_stop() {
    let fake = Arc::new(Fake::default());
    let h = harness_with_ocr(fake.clone());
    h.core.config.write().ocr.local_processing = false;
    let admin = h.admin();
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/remove",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(fake.steps.lock().as_slice(), ["stopped", "remove"]);
    assert_eq!(r.json()["local_turned_off"], false);
    assert_eq!(r.json()["returned"], 0);
}

/// A removal the machine refuses (an install runs) changes nothing: OCR on this machine
/// stays on.
#[tokio::test]
async fn a_refused_remove_leaves_ocr_on_this_machine_on() {
    let fake = Arc::new(Fake {
        refuse: Some("An install is running: wait for it to end".into()),
        ..Fake::default()
    });
    let h = harness_with_ocr(fake.clone());
    let admin = h.admin();
    let r = h
        .call(
            "POST",
            "/_admin/api/machine/remove",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 409);
    assert!(r.json()["error"].as_str().unwrap().contains("install"));
    assert!(fake.steps.lock().is_empty(), "{:?}", fake.steps.lock());
    assert!(h.core.config.read().ocr.local_processing);
}
