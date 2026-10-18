//! The setup wizard's "No library here: read OCR for another Bunko" (full build):
//! `/setup/api/processor/{test,setup}` behind the setup gate, answered by the machine.

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_server::machine::{Machine, NO_PROCESSOR};
use serde_json::{Value, json};
use std::sync::Arc;

/// A machine that can (`full`) or cannot (lite) run a processor; it records the forms.
#[derive(Default)]
struct FakeMachine {
    full: bool,
    tests: parking_lot::Mutex<Vec<Value>>,
    setups: parking_lot::Mutex<Vec<Value>>,
}

impl Machine for FakeMachine {
    fn overview(&self) -> Value {
        json!({})
    }
    fn setup_options(&self) -> Value {
        if self.full {
            json!({"choices": []})
        } else {
            Value::Null
        }
    }
    fn backend_locked(&self) -> Option<String> {
        None
    }
    fn ocr_changed(&self, _: &bunko_core::Config, _: bool, _: bool) -> Value {
        json!({})
    }
    fn install(&self, _: bool) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn remove(&self) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn start_job(&self, _: &str, _: Option<&str>) -> Result<Value, String> {
        Err("no".into())
    }
    fn job(&self, _: u64, _: u64) -> Option<Value> {
        None
    }
    fn logs(&self, _: usize) -> Value {
        json!({})
    }
    fn processor_test(&self, form: &Value) -> Result<Value, String> {
        if !self.full {
            return Err(NO_PROCESSOR.into());
        }
        self.tests.lock().push(form.clone());
        if form["password"] == "wrong" {
            return Err("the library refused the processor account: wrong password".into());
        }
        Ok(json!({"url": form["url"], "username": form["username"], "notes": []}))
    }
    fn processor_setup(&self, form: &Value) -> Result<Value, String> {
        if !self.full {
            return Err(NO_PROCESSOR.into());
        }
        self.setups.lock().push(form.clone());
        Ok(json!({"url": form["url"], "name": "this-pc", "stopping": true}))
    }
}

fn env_with(full: bool) -> (Env, Arc<FakeMachine>) {
    let mut env = Env::new();
    let machine = Arc::new(FakeMachine {
        full,
        ..FakeMachine::default()
    });
    env.deps.machine = Some(machine.clone());
    (env, machine)
}

fn form(password: &str) -> Value {
    json!({"url": "https://library.example", "username": "proc", "password": password,
           "tls_verify": "true"})
}

#[tokio::test]
async fn the_processor_step_is_gated_like_the_rest_of_the_setup() {
    let (env, machine) = env_with(true);
    // From another computer without the setup code: refused, the machine never asked.
    let r = env
        .send(json_body(
            from_peer("POST", "/setup/api/processor/setup", "203.0.113.10"),
            form("pw"),
        ))
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    // A cross-site form cannot send one.
    let r = env
        .send(raw(
            req("POST", "/setup/api/processor/test").header("content-type", "text/plain"),
            br#"{"url":"https://library.example","username":"proc","password":"pw"}"#,
        ))
        .await;
    assert_eq!(r.status, 415, "{}", r.text());
    assert!(machine.tests.lock().is_empty() && machine.setups.lock().is_empty());
    // Once an admin exists the wizard is over.
    env.user("admin", "password123", Role::Admin);
    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/test"),
            form("pw"),
        ))
        .await;
    assert_eq!(r.status, 410, "{}", r.text());
}

#[tokio::test]
async fn a_lite_build_or_no_machine_refuses_the_processor_step() {
    let (env, _) = env_with(false);
    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/test"),
            form("pw"),
        ))
        .await;
    assert_eq!((r.status, r.json()), (400, json!({"error": NO_PROCESSOR})));
    let env = Env::new();
    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/setup"),
            form("pw"),
        ))
        .await;
    assert_eq!((r.status, r.json()), (400, json!({"error": NO_PROCESSOR})));
}

#[tokio::test]
async fn test_answers_the_librarys_verdict_and_setup_hands_over_to_the_machine() {
    let (env, machine) = env_with(true);
    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/test"),
            form("wrong"),
        ))
        .await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .contains("wrong password")
    );
    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/test"),
            form("pw"),
        ))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["username"], "proc");
    assert!(machine.setups.lock().is_empty(), "a test writes nothing");

    let r = env
        .send(json_body(
            req("POST", "/setup/api/processor/setup"),
            form("pw"),
        ))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["stopping"], true);
    let setups = machine.setups.lock().clone();
    assert_eq!(setups.len(), 1);
    assert_eq!(setups[0]["password"], "pw");
    assert_eq!(setups[0]["url"], "https://library.example");
    // No admin was made: this server was never set up as a library.
    assert!(env.db.get_user("admin").unwrap().is_none());
}
