//! `GET /_admin/api/update` and `POST /_admin/api/update/apply` over a fake
//! `UpdateSource`.

mod admin_support;

use admin_support::{Harness, Options};
use bunko_core::Config;
use bunko_server::admin::UpdateSource;
use bunko_update::{InstallKind, UpdateStatus};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct Fake {
    install: InstallKind,
    latest: &'static str,
    can_apply: bool,
    apply_result: Mutex<Result<String, String>>,
    checks: AtomicUsize,
    applies: AtomicUsize,
}

impl Fake {
    fn new(install: InstallKind, latest: &'static str, can_apply: bool) -> Arc<Fake> {
        Arc::new(Fake {
            install,
            latest,
            can_apply,
            apply_result: Mutex::new(Ok(latest.to_string())),
            checks: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
        })
    }
}

impl UpdateSource for Fake {
    fn check(&self) -> BoxFuture<'_, UpdateStatus> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        let available = self.latest != bunko_core::VERSION;
        Box::pin(async move {
            UpdateStatus {
                current: bunko_core::VERSION.into(),
                latest: Some(self.latest.into()),
                available,
                notes_url: Some("https://example.invalid/notes".into()),
                install: self.install.clone(),
                can_apply: available && self.can_apply,
                docker_image: Some("ghcr.io/example/bunko:9.9.9".into()),
                checked_at: Some("2026-10-01T00:00:00Z".into()),
                error: None,
                note: None,
            }
        })
    }
    fn apply(&self) -> BoxFuture<'_, Result<String, String>> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        let r = self.apply_result.lock().clone();
        Box::pin(async move { r })
    }
}

fn harness(fake: Arc<Fake>, check: bool) -> Harness {
    Harness::with(Options {
        updates: Some(fake),
        configure: Some(Box::new(move |c: &mut Config| c.update.check = check)),
        ..Options::default()
    })
}

fn self_managed() -> InstallKind {
    InstallKind::SelfManaged {
        exe: "/opt/bunko/mokuro-bunko".into(),
    }
}

#[tokio::test]
async fn status_is_cached_and_refreshable() {
    let fake = Fake::new(self_managed(), "9.9.9", true);
    let h = harness(fake.clone(), true);
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/update", Some(&admin), None)
        .await;
    assert_eq!(r.status, 200);
    let s = r.json();
    assert_eq!(s["current"], bunko_core::VERSION);
    assert_eq!(s["latest"], "9.9.9");
    assert_eq!(s["available"], true);
    assert_eq!(s["can_apply"], true);
    assert_eq!(s["notes_url"], "https://example.invalid/notes");
    assert_eq!(s["install"]["kind"], "self_managed");
    assert_eq!(s["checks_enabled"], true);
    assert_eq!(s["applying"], false);
    h.call("GET", "/_admin/api/update", Some(&admin), None)
        .await;
    assert_eq!(
        fake.checks.load(Ordering::SeqCst),
        1,
        "served from the cache"
    );
    h.call("GET", "/_admin/api/update?refresh=1", Some(&admin), None)
        .await;
    assert_eq!(fake.checks.load(Ordering::SeqCst), 2);

    // Admins only.
    let ivy = h.login("ivy", bunko_core::Role::Inviter);
    let r = h.call("GET", "/_admin/api/update", Some(&ivy), None).await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn checks_off_means_no_contact_until_asked() {
    let fake = Fake::new(self_managed(), "9.9.9", true);
    let h = harness(fake.clone(), false);
    let admin = h.admin();
    let s = h
        .call("GET", "/_admin/api/update", Some(&admin), None)
        .await
        .json();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    assert_eq!(s["checks_enabled"], false);
    assert_eq!(s["latest"], Value::Null);
    assert_eq!(s["checked_at"], Value::Null);
    let s = h
        .call("GET", "/_admin/api/update?refresh=1", Some(&admin), None)
        .await
        .json();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    assert_eq!(s["latest"], "9.9.9");
}

#[tokio::test]
async fn apply_installs_audits_and_restarts() {
    let fake = Fake::new(self_managed(), "9.9.9", true);
    let h = harness(fake.clone(), true);
    let admin = h.admin();
    let r = h
        .call(
            "POST",
            "/_admin/api/update/apply",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let body = r.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], "9.9.9");
    assert_eq!(body["restarting"], true);
    assert_eq!(fake.applies.load(Ordering::SeqCst), 1);
    let ev = &h.audit()[0];
    assert_eq!(ev.action, "admin_update_server");
    assert_eq!(
        ev.details.as_deref(),
        Some(format!(r#"{{"from":"{}","to":"9.9.9"}}"#, bunko_core::VERSION).as_str())
    );
    // The restart comes after the response.
    assert_eq!(h.restarts.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.restarts.load(Ordering::SeqCst), 1);
    // While restarting, a second apply is refused.
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (409, json!({"error": "An update is already being applied"}))
    );
}

#[tokio::test]
async fn apply_refusals_and_failures() {
    let docker = Fake::new(InstallKind::Docker, "9.9.9", false);
    let h = harness(docker.clone(), true);
    let admin = h.admin();
    let s = h
        .call("GET", "/_admin/api/update", Some(&admin), None)
        .await
        .json();
    assert_eq!(s["can_apply"], false);
    assert_eq!(s["docker_image"], "ghcr.io/example/bunko:9.9.9");
    assert!(
        s["cannot_apply_reason"]
            .as_str()
            .unwrap()
            .contains("pull ghcr.io/example/bunko:9.9.9")
    );
    // No automatic updates in Docker: the page says how instead (with the image), and
    // a saved `update.auto: true` is accepted and ignored.
    assert_eq!(s["auto_supported"], false);
    let manual = s["manual_update"].as_str().unwrap();
    assert!(
        manual.starts_with("Runs in Docker: update by pulling the new image")
            && manual.ends_with("Image: ghcr.io/example/bunko:9.9.9"),
        "{manual}"
    );
    let r = h
        .call(
            "POST",
            "/_admin/api/update/settings",
            Some(&admin),
            Some(json!({"auto": true})),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["auto"], false);
    let s = h
        .call("GET", "/_admin/api/update", Some(&admin), None)
        .await
        .json();
    assert_eq!(
        (s["auto"].clone(), s["problems"].clone()),
        (json!(false), json!([]))
    );
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(r.status, 409);
    assert!(r.json()["error"].as_str().unwrap().contains("Docker"));
    assert_eq!(docker.applies.load(Ordering::SeqCst), 0);

    let current = Fake::new(self_managed(), bunko_core::VERSION, true);
    let h = harness(current, true);
    let admin = h.admin();
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (409, json!({"error": "No newer release is available"}))
    );

    let failing = Fake::new(self_managed(), "9.9.9", true);
    *failing.apply_result.lock() =
        Err("the download's sha256 is 00, the signed manifest says ff".into());
    let h = harness(failing.clone(), true);
    let admin = h.admin();
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(r.status, 500);
    assert!(r.json()["error"].as_str().unwrap().contains("sha256"));
    // A failure releases the guard: the admin may retry.
    *failing.apply_result.lock() = Ok("9.9.9".into());
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(h.restarts.load(Ordering::SeqCst), 0, "not yet");
}

#[tokio::test]
async fn without_an_update_service_the_endpoints_are_absent() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/update", Some(&admin), None)
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (404, json!({"error": "API endpoint not found"}))
    );
    let r = h
        .call("POST", "/_admin/api/update/apply", Some(&admin), None)
        .await;
    assert_eq!(r.status, 404);
}
