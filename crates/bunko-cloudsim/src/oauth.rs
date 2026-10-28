//! The OAuth token endpoints, `POST {base}/oauth/{google|microsoft}/token`: the
//! `refresh_token` grant only (sign-in itself is a live-run concern). Answers as the
//! providers do: Google without a refresh token, Microsoft with a rotated one; a
//! revoked or unknown refresh token is `400 {"error":"invalid_grant"}`.

use std::collections::HashMap;

use axum::Form;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::SimState;
use crate::accounts::{EXPIRES_IN, Provider};
use crate::log::LogTag;

fn oauth_err(error: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": error, "error_description": description})),
    )
        .into_response()
}

pub async fn token(
    State(sim): State<SimState>,
    Path(provider): Path<String>,
    form: Result<Form<HashMap<String, String>>, FormRejection>,
) -> Response {
    let p = Provider::parse(&provider).unwrap_or(Provider::Google);
    let (account, mut r) = answer(&sim, &provider, form);
    r.extensions_mut().insert(LogTag {
        provider: p,
        account,
        class: "auth",
        bytes_in: 0,
    });
    r
}

fn answer(
    sim: &SimState,
    provider: &str,
    form: Result<Form<HashMap<String, String>>, FormRejection>,
) -> (String, Response) {
    let path = format!("/oauth/{provider}/token");
    let bad = |what: String| {
        sim.violation(format!("auth POST {path}: {what}"));
        (String::new(), oauth_err("invalid_request", &what))
    };
    let p = match Provider::parse(provider) {
        Some(p @ (Provider::Google | Provider::Microsoft)) => p,
        _ => return bad(format!("no OAuth endpoint for {provider:?}")),
    };
    let Form(f) = match form {
        Ok(f) => f,
        Err(e) => return bad(format!("not a form body: {e}")),
    };
    match f.get("grant_type").map(String::as_str) {
        Some("refresh_token") => {}
        other => {
            return bad(format!(
                "grant_type {other:?} (only refresh_token is simulated)"
            ));
        }
    }
    let Some(rt) = f.get("refresh_token") else {
        return bad("a refresh without refresh_token".into());
    };
    if f.get("client_id").is_none_or(|c| c.is_empty()) {
        return bad("a refresh without client_id".into());
    }
    let res = sim.tokens().refresh(p, rt);
    match res {
        Ok((access_token, rotated)) => {
            let account = sim.tokens().resolve(p, &access_token).unwrap_or_default();
            let mut v = json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": EXPIRES_IN,
                "scope": f.get("scope").cloned().unwrap_or_default(),
            });
            if let Some(r) = rotated {
                v["refresh_token"] = json!(r);
            }
            (account, axum::Json(v).into_response())
        }
        Err(_) => (
            String::new(),
            oauth_err("invalid_grant", "Token has been expired or revoked."),
        ),
    }
}
