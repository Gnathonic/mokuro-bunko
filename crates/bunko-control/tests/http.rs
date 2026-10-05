//! The control listener over real HTTP: loopback only, token required, pause/resume,
//! SSE, stop only for managed instances, `.control.json` written 0600 and removed.

use std::time::Duration;

use bunko_control::{
    CONTROL_FILE, Control, ControlConfig, ControlListener, PauseMode, Role, State, Status,
    read_control_file,
};
use tokio_util::sync::CancellationToken;

fn config(storage: &std::path::Path, role: Role, managed: bool) -> ControlConfig {
    let mut c = ControlConfig::new(role, "box", "0.7.0-test", storage);
    c.managed = managed;
    c
}

async fn start(
    storage: &std::path::Path,
    role: Role,
    managed: bool,
) -> (Control, bunko_control::ControlServer, CancellationToken) {
    let control = Control::new(config(storage, role, managed));
    control.set_link(
        bunko_control::LinkPhase::Connected,
        Some("http://lib"),
        None,
    );
    let stop = CancellationToken::new();
    control.set_stop(stop.clone());
    let listener = ControlListener::bind().await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.ip().is_loopback(), "bound to {addr}");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    let server = listener.serve(control.clone(), None).unwrap();
    (control, server, stop)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn token_required_and_control_file_written_then_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (_control, server, _stop) = start(dir.path(), Role::Processor, false).await;
    let file = read_control_file(dir.path()).expect(".control.json");
    assert_eq!(file.port, server.port());
    assert_eq!(file.token, server.token());
    assert_eq!(file.role, Role::Processor);
    assert_eq!(file.pid, std::process::id());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(CONTROL_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let url = format!("{}/control/status", server.url());
    let c = client();
    assert_eq!(c.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        c.get(&url)
            .bearer_auth("not-the-token")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let ok = c.get(&url).bearer_auth(&file.token).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let status: Status = ok.json().await.unwrap();
    assert_eq!(status.role, Role::Processor);
    assert_eq!(status.state, State::Idle);
    assert!(status.can_pause);
    // A foreign Host (DNS rebinding) is refused even with the token.
    let foreign = c
        .get(&url)
        .bearer_auth(&file.token)
        .header("host", format!("evil.example:{}", server.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 403);
    server.shutdown().await;
    assert!(!dir.path().join(CONTROL_FILE).exists());
}

#[tokio::test]
async fn login_sets_the_cookie_and_the_cookie_authorizes() {
    let dir = tempfile::tempdir().unwrap();
    let (_control, server, _stop) = start(dir.path(), Role::Gui, false).await;
    let c = client();
    let bad = c
        .get(format!("{}/app/login?t=nope", server.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    let login = c
        .get(server.login_url(Some("/app/dashboard")))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    assert_eq!(login.headers()["location"], "/app/dashboard");
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    let pair = cookie.split(';').next().unwrap().to_string();
    let status = c
        .get(format!("{}/control/status", server.url()))
        .header("cookie", &pair)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let s: Status = status.json().await.unwrap();
    assert_eq!(s.state, State::Setup);
    assert!(!s.can_pause);
    // A cookie-authenticated POST from another origin is refused; from ours it passes
    // the guard (gui has nothing to pause: 409).
    let cross = c
        .post(format!("{}/control/resume", server.url()))
        .header("cookie", &pair)
        .header("origin", "http://127.0.0.1:1")
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403);
    let same = c
        .post(format!("{}/control/resume", server.url()))
        .header("cookie", &pair)
        .header("origin", server.url())
        .send()
        .await
        .unwrap();
    assert_eq!(same.status(), 409);
    server.shutdown().await;
}

#[tokio::test]
async fn pause_resume_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let (control, server, _stop) = start(dir.path(), Role::Processor, false).await;
    let c = client();
    let base = server.url().to_string();
    let token = server.token().to_string();
    // The event stream: a status at once.
    let resp = c
        .get(format!("{base}/control/events"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let mut events = Stream {
        resp,
        buf: String::new(),
    };
    let first = next_status(&mut events).await;
    assert_eq!(first.state, State::Idle);
    // Bad bodies.
    let bad = c
        .post(format!("{base}/control/pause"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"mode": "sometime"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let past = c
        .post(format!("{base}/control/pause"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"mode": "now", "until": "2001-01-01T00:00:00Z"}))
        .send()
        .await
        .unwrap();
    assert_eq!(past.status(), 400);
    // Pause until: answered with the new status; the stream tells too.
    let until = chrono::Utc::now() + chrono::Duration::seconds(3);
    let until_text = until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let paused: Status = c
        .post(format!("{base}/control/pause"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"mode": "after_volume", "until": until_text}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(paused.state, State::Paused);
    assert_eq!(paused.pause.mode, Some(PauseMode::AfterVolume));
    assert_eq!(paused.pause.until.as_deref(), Some(until_text.as_str()));
    assert!(dir.path().join(bunko_control::PAUSE_FILE).is_file());
    let told = next_status(&mut events).await;
    assert_eq!(told.state, State::Paused);
    // ... and the pause lifts itself at `until`, which the stream reports.
    let lifted = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let s = next_status(&mut events).await;
            if s.state == State::Idle {
                return s;
            }
        }
    })
    .await
    .expect("the pause lifted itself");
    assert_eq!(lifted.pause.mode, None);
    assert!(!dir.path().join(bunko_control::PAUSE_FILE).exists());
    // Pause now + resume.
    control.pause_mode(PauseMode::Now, None).unwrap();
    assert_eq!(next_status(&mut events).await.state, State::Paused);
    let resumed: Status = c
        .post(format!("{base}/control/resume"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resumed.state, State::Idle);
    assert_eq!(next_status(&mut events).await.state, State::Idle);
    // Shutting down ends the stream.
    server.shutdown().await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.chunk().await {
                Ok(Some(_)) => continue,
                _ => return,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the event stream ended with the listener");
}

#[tokio::test]
async fn stop_only_for_managed_instances() {
    let dir = tempfile::tempdir().unwrap();
    let (_c, server, stop) = start(dir.path(), Role::Processor, false).await;
    let resp = client()
        .post(format!("{}/control/stop", server.url()))
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert!(!stop.is_cancelled());
    server.shutdown().await;

    let dir = tempfile::tempdir().unwrap();
    let (_c, server, stop) = start(dir.path(), Role::Processor, true).await;
    let resp = client()
        .post(format!("{}/control/stop", server.url()))
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    assert!(stop.is_cancelled());
    server.shutdown().await;
}

#[tokio::test]
async fn a_lite_server_has_nothing_to_pause() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), Role::Server, false);
    cfg.ocr = false;
    let control = Control::new(cfg);
    let listener = ControlListener::bind().await.unwrap();
    let server = listener.serve(control, None).unwrap();
    let resp = client()
        .post(format!("{}/control/pause", server.url()))
        .bearer_auth(server.token())
        .json(&serde_json::json!({"mode": "now"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    server.shutdown().await;
}

/// A second live instance on the same storage never overwrites the first's
/// `.control.json`, except that a server/processor takes over from the setup app.
#[tokio::test]
async fn one_live_owner_per_storage() {
    let dir = tempfile::tempdir().unwrap();
    // Another process's live listener (a foreign pid, a port that answers).
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = other.local_addr().unwrap().port();
    let foreign = |role: Role| bunko_control::ControlFile {
        role,
        pid: std::process::id().wrapping_add(1),
        port,
        token: "theirs".into(),
        version: "0.7.0".into(),
        started_at: "2026-10-04T10:00:00Z".into(),
        url: format!("http://127.0.0.1:{port}"),
        managed: false,
    };
    bunko_control::file::write_control_file(dir.path(), &foreign(Role::Processor)).unwrap();
    let control = Control::new(config(dir.path(), Role::Processor, false));
    let err = ControlListener::bind()
        .await
        .unwrap()
        .serve(control.clone(), None)
        .err()
        .expect("refused while the other lives");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(read_control_file(dir.path()).unwrap().token, "theirs");

    // The setup app's file: a gui is refused, a processor takes over.
    bunko_control::file::write_control_file(dir.path(), &foreign(Role::Gui)).unwrap();
    let gui = Control::new(config(dir.path(), Role::Gui, false));
    assert!(
        ControlListener::bind()
            .await
            .unwrap()
            .serve(gui, None)
            .is_err()
    );
    let server = ControlListener::bind()
        .await
        .unwrap()
        .serve(control, None)
        .unwrap();
    assert_eq!(read_control_file(dir.path()).unwrap().token, server.token());
    server.shutdown().await;

    // A stale file (nothing listening) is simply replaced.
    drop(other);
    bunko_control::file::write_control_file(dir.path(), &foreign(Role::Server)).unwrap();
    let control = Control::new(config(dir.path(), Role::Server, false));
    let server = ControlListener::bind()
        .await
        .unwrap()
        .serve(control, None)
        .unwrap();
    assert_eq!(read_control_file(dir.path()).unwrap().token, server.token());
    server.shutdown().await;
}

/// An SSE response and what was read of it but not consumed.
struct Stream {
    resp: reqwest::Response,
    buf: String,
}

impl Stream {
    async fn chunk(&mut self) -> reqwest::Result<Option<bytes::Bytes>> {
        self.resp.chunk().await
    }
}

/// The next `status` event off an SSE response.
async fn next_status(stream: &mut Stream) -> Status {
    let Stream { resp, buf } = stream;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                if let Some(data) = block
                    .lines()
                    .find_map(|l| l.strip_prefix("data:"))
                    .map(str::trim)
                {
                    assert!(block.contains("event: status"), "{block}");
                    return serde_json::from_str::<Status>(data).unwrap();
                }
                continue;
            }
            let chunk = resp.chunk().await.unwrap().expect("stream open");
            buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    })
    .await
    .expect("a status event")
}
