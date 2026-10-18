//! The setup wizard's "Where does your library live?", Connect and Check steps (0.8
//! §11 steps 2-4): `/setup/api/storage...`, behind the same gate as the rest of the
//! setup API (this machine, a setup session, or the setup code), and only while setup
//! is needed (410 after: the admin Storage panel does it then).
//!
//! A Connect writes the credentials and `storage.library` at once; the server opens
//! the store when the wizard finishes (it restarts then, `complete`). POSTs must be
//! JSON (`Content-Type: application/json`), so no cross-site form can send one.

use super::*;
use crate::storage::StorageControl;
use crate::storage::connect::{ConnectRequest, RequestOrigin};

pub(super) fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/setup/api/storage", get(status))
        .route(
            "/setup/api/storage/connect",
            get(connect_status).post(connect),
        )
        .route("/setup/api/storage/connect/cancel", post(cancel))
        .route("/setup/api/storage/connect/paste", post(paste))
        .route("/setup/api/storage/connect/account", post(account))
        .route("/setup/api/storage/connect/create", post(create))
        .route("/setup/api/storage/local", post(local))
}

/// The gate: setup still needed, and this caller may run it.
async fn allowed(
    d: &AccountsDeps,
    client: &Client,
    parts: &Parts,
) -> Result<Arc<StorageControl>, Response> {
    match needs_setup(d).await {
        Err(resp) => return Err(resp),
        Ok(false) => {
            return Err(json_error(
                410,
                "Setup already completed: the admin panel's Storage tab connects the library",
            ));
        }
        Ok(true) => {}
    }
    match gate_of(d, client, parts) {
        Gate::Allowed => {}
        g => return Err(refusal(g)),
    }
    d.storage
        .clone()
        .ok_or_else(|| json_error(503, "This server cannot change where its library lives"))
}

/// A POST's JSON object body (`{}` when empty), refusing any other content type.
pub(super) async fn json_body(
    parts: &Parts,
    body: Body,
) -> Result<serde_json::Map<String, Value>, Response> {
    let ct = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("application/json")
    {
        return Err(json_error(415, "Send the request as JSON"));
    }
    match read_body(&parts.headers, body).await {
        JsonBody::Empty => Ok(serde_json::Map::new()),
        JsonBody::TooLarge => Err(json_error(413, "Request body too large")),
        JsonBody::Bytes(b) => {
            super::super::util::parse_object(&b).ok_or_else(|| json_error(400, "Invalid JSON"))
        }
    }
}

fn origin(client: &Client, parts: &Parts) -> RequestOrigin {
    let ua = parts
        .headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    RequestOrigin::of(is_local(client), ua)
}

/// `GET /setup/api/storage`: the configured library, the mounted store and the
/// sign-in in progress.
async fn status(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match blocking(move || c.status_json()).await {
        Ok(v) => json_response(200, v),
        Err(r) => r,
    }
}

/// `POST /setup/api/storage/connect` `{provider, reader?, folder?, flow?, webdav?,
/// mega?}`: start the sign-in (the flow is chosen from where this request comes from).
async fn connect(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match json_body(&parts, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: ConnectRequest = match serde_json::from_value(Value::Object(body)) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("Invalid connect request: {e}")),
    };
    info!("setup: connecting the library to {}", req.provider);
    match c.connect(req, origin(&client, &parts), false) {
        Ok(st) => json_response(200, st.as_json()),
        Err(e) => json_error(400, &e),
    }
}

async fn connect_status(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    match allowed(&d, &client, &parts).await {
        Ok(c) => json_response(200, c.connector().status().as_json()),
        Err(r) => r,
    }
}

async fn cancel(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = json_body(&parts, body).await {
        return r;
    }
    c.connector().cancel();
    json_response(200, c.connector().status().as_json())
}

/// `POST /setup/api/storage/connect/paste` `{url}`: the address the browser ended on
/// after a `paste` sign-in (never logged). 400 when it is not this sign-in's redirect;
/// the sign-in then waits for another.
async fn paste(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match json_body(&parts, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return json_error(400, "url (the address the browser ended on) is required");
    };
    match c.connector().paste(url.to_string()).await {
        Ok(st) => json_response(200, st.as_json()),
        Err(e) => json_error(400, &e),
    }
}

/// `POST /setup/api/storage/connect/account` `{use_this_account: bool}`.
async fn account(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match json_body(&parts, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let yes = body
        .get("use_this_account")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match c.decide_account(yes, false).await {
        Ok(st) => json_response(200, st.as_json()),
        Err(e) => json_error(400, &e),
    }
}

/// `POST /setup/api/storage/connect/create`: "Create it?".
async fn create(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = json_body(&parts, body).await {
        return r;
    }
    match c.connector().create_folder().await {
        Ok(check) => json_response(200, json!({"check": check})),
        Err(e) => json_error(400, &e),
    }
}

/// `POST /setup/api/storage/local`: "This computer".
async fn local(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    let c = match allowed(&d, &client, &parts).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = json_body(&parts, body).await {
        return r;
    }
    c.connector().cancel();
    let c2 = c.clone();
    match blocking(move || c2.use_local()).await {
        Ok(Ok(())) => {
            let lib = c.config().storage.layout().library();
            json_response(200, json!({"backend": "local", "folder": lib}))
        }
        Ok(Err(e)) => json_error(409, &e),
        Err(r) => r,
    }
}
