//! The setup wizard's "No library here: this computer reads OCR for another Bunko"
//! (full build): `/setup/api/processor/{test,setup}`, behind the same gate as the
//! rest of the setup API and only while setup is needed. The machine checks the
//! processor account against that library, writes `processor.yaml`, starts the
//! processor and retires this setup-only server ([`crate::machine::Machine`]).
//! POSTs must be JSON, so no cross-site form can send one.

use super::*;
use crate::machine::{Machine, NO_PROCESSOR};

pub(super) fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/setup/api/processor/test", post(test))
        .route("/setup/api/processor/setup", post(setup))
}

/// The gate: setup still needed, this caller may run it, and a machine that can run
/// a processor (not the lite build).
async fn allowed(
    d: &AccountsDeps,
    client: &Client,
    parts: &Parts,
) -> Result<Arc<dyn Machine>, Response> {
    match needs_setup(d).await {
        Err(resp) => return Err(resp),
        Ok(false) => return Err(json_error(410, "Setup already completed")),
        Ok(true) => {}
    }
    match gate_of(d, client, parts) {
        Gate::Allowed => {}
        g => return Err(refusal(g)),
    }
    d.machine
        .clone()
        .ok_or_else(|| json_error(400, NO_PROCESSOR))
}

async fn run(d: AccountsDeps, client: Client, parts: Parts, body: Body, finish: bool) -> Response {
    // One finish at a time: two must not both write processor.yaml and start one.
    static FINISHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = if finish {
        Some(FINISHING.lock().await)
    } else {
        None
    };
    let machine = match allowed(&d, &client, &parts).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let form = match super::storage::json_body(&parts, body).await {
        Ok(m) => Value::Object(m),
        Err(r) => return r,
    };
    let answer = blocking(move || {
        if finish {
            machine.processor_setup(&form)
        } else {
            machine.processor_test(&form)
        }
    })
    .await;
    match answer {
        Ok(Ok(v)) => {
            if finish {
                info!("setup: this computer now reads OCR for another library");
            }
            json_response(200, v)
        }
        Ok(Err(e)) => json_error(400, &e),
        Err(resp) => resp,
    }
}

/// `POST /setup/api/processor/test` `{url, username, password, tls_verify}`: log in
/// to that library with the processor account (nothing is written).
async fn test(State(d): State<AccountsDeps>, client: Client, parts: Parts, body: Body) -> Response {
    run(d, client, parts, body, false).await
}

/// `POST /setup/api/processor/setup` (the same, plus `overwrite`): write
/// `processor.yaml`, start the processor; this server stops after answering.
async fn setup(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    run(d, client, parts, body, true).await
}
