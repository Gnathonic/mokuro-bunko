//! The setup wizard's account check and file writing against a fake library.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bunko_processor::config::TlsVerify;
use bunko_processor::setup::{Prompter, SetupOptions, run, verify_account};
use parking_lot::Mutex;
use serde_json::json;

#[derive(Clone)]
struct Lib {
    role: &'static str,
    protocols: Vec<u32>,
    version: &'static str,
    register_status: u16,
    me_redirect: bool,
    probes: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn me(State(lib): State<Lib>, headers: HeaderMap) -> Response {
    if lib.me_redirect {
        return (
            StatusCode::FOUND,
            [(header::LOCATION, "https://elsewhere.example/login/api/me")],
        )
            .into_response();
    }
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some("Basic Z3B1OnB3")
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "Invalid credentials"})),
        )
            .into_response();
    }
    axum::Json(json!({"authenticated": true, "role": lib.role, "username": "gpu"})).into_response()
}

async fn probe(
    State(lib): State<Lib>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    lib.probes.lock().push(body);
    (
        StatusCode::from_u16(lib.register_status).unwrap(),
        axum::Json(json!({"error": "this server speaks protocol 3, not 0", "protocols": lib.protocols, "version": lib.version})),
    )
        .into_response()
}

async fn library(lib: Lib) -> String {
    let app = Router::new()
        .route("/login/api/me", get(me))
        .route("/_processor/register", post(probe))
        .with_state(lib);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn lib() -> Lib {
    Lib {
        role: "processor",
        protocols: vec![3],
        version: env!("CARGO_PKG_VERSION"),
        register_status: 400,
        me_redirect: false,
        probes: Arc::default(),
    }
}

#[tokio::test]
async fn the_account_check_reads_role_and_protocol_without_registering() {
    let fake = lib();
    let probes = fake.probes.clone();
    let url = library(fake).await;
    let ok = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap();
    assert_eq!(
        (ok.username.as_str(), ok.role.as_str(), ok.protocol),
        ("gpu", "processor", 3)
    );
    assert!(ok.notes.is_empty(), "{:?}", ok.notes);
    assert_eq!(probes.lock().as_slice(), [json!({"protocol": 0})]);

    let e = verify_account(&url, "gpu", "wrong", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(
        e.0.starts_with("The library refused the username 'gpu' or its password"),
        "{e}"
    );

    let url = library(Lib {
        role: "admin",
        ..lib()
    })
    .await;
    let e = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(
        e.0.contains("'gpu' is an admin account, not a processor account"),
        "{e}"
    );

    let url = library(Lib {
        protocols: vec![4],
        version: "0.8.0",
        ..lib()
    })
    .await;
    let e = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(
        e.0.contains("the library (mokuro-bunko 0.8.0) speaks 4. Update this machine"),
        "{e}"
    );

    let url = library(Lib {
        version: "0.7.9",
        ..lib()
    })
    .await;
    let ok = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap();
    assert!(
        ok.notes[0].contains("and the library 0.7.9"),
        "{:?}",
        ok.notes
    );

    let url = library(Lib {
        register_status: 404,
        ..lib()
    })
    .await;
    let e = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(e.0.contains("has no remote processors"), "{e}");

    let url = library(Lib {
        me_redirect: true,
        ..lib()
    })
    .await;
    let e = verify_account(&url, "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(
        e.0.contains("redirects to https://elsewhere.example: run setup again"),
        "{e}"
    );

    let e = verify_account("http://127.0.0.1:9", "gpu", "pw", &TlsVerify::Yes)
        .await
        .unwrap_err();
    assert!(
        e.0.starts_with("Could not reach the library at http://127.0.0.1:9"),
        "{e}"
    );
}

struct Script {
    answers: Vec<String>,
    said: Vec<String>,
}

impl Prompter for Script {
    fn say(&mut self, line: &str) {
        self.said.push(line.to_string());
    }
    fn ask(&mut self, prompt: &str, _hidden: bool) -> std::io::Result<String> {
        self.said.push(format!("? {prompt}"));
        Ok(self.answers.remove(0))
    }
    fn confirm(&mut self, _question: &str, default: bool) -> std::io::Result<bool> {
        Ok(default)
    }
}

#[tokio::test]
async fn the_wizard_checks_then_writes_only_non_defaults() {
    let url = library(lib()).await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("processor.yaml");
    let options = SetupOptions {
        config: config.clone(),
        url: Some(url.trim_start_matches("http://").to_string()),
        name: Some("tower-x".into()),
        command: "mokuro-bunko".into(),
        ..Default::default()
    };
    let mut ui = Script {
        answers: vec!["gpu".into(), "pw".into()],
        said: Vec::new(),
    };
    run(&options, &mut ui, None).await.unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains(&format!("url: {url}")), "{text}");
    assert!(text.contains("name: tower-x"));
    assert!(!text.contains("tls_verify"));
    assert!(ui.said.iter().any(|l| l.starts_with("No scheme given")));
    assert!(
        ui.said
            .iter()
            .any(|l| l
                .starts_with("Logged in: gpu is a processor account (protocol 3, mokuro-bunko ")),
        "{:?}",
        ui.said
    );
    assert!(
        ui.said
            .iter()
            .any(|l| l.contains("processor serve --config"))
    );

    // --yes with a file there and no --force writes nothing.
    let again = SetupOptions {
        yes: true,
        url: Some(url.clone()),
        username: Some("gpu".into()),
        password: Some("pw".into()),
        ..options
    };
    let e = run(
        &again,
        &mut Script {
            answers: vec![],
            said: vec![],
        },
        None,
    )
    .await
    .unwrap_err();
    assert!(e.0.contains("already exists; pass --force"), "{e}");
}
