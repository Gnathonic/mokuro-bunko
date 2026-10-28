//! The reader side of the contract (spec interop-testing §5): every request the reader
//! recorded (`interop/contract/reader/*.json`, written by its own tests) and the
//! hand-written legacy ones (`reader/legacy/*.json`) are sent to a freshly seeded
//! server, and the reader's expectations are checked: the status, the response fields
//! its code reads (present, or present and possibly null, or with a given value), the
//! headers. Each request goes as a browser on the reader's origin sends it: with
//! `Origin`, after a preflight when it is not simple; the preflight must allow every
//! header it sends and the answer must expose every header it reads. Every broken record
//! is reported at once.

// Records are read with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod contract_support;

use std::path::Path;

use contract_support::{ETAG_SENTINEL, Req, Resp, Seed, Server, contract_dir};
use serde_json::Value;

struct Record {
    name: String,
    req: Req,
    expect: Value,
    after: Option<String>,
}

fn load(dir: &Path, prefix: &str, out: &mut Vec<Record>, errors: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let name = format!("{prefix}{stem}");
        let parsed = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
            .and_then(|v| {
                let req = Req::from_json(&v["request"])?;
                Ok(Record {
                    name: name.clone(),
                    req,
                    expect: v["expect"].clone(),
                    after: v["after"].as_str().map(|a| format!("{prefix}{a}")),
                })
            });
        match parsed {
            Ok(r) => out.push(r),
            Err(e) => errors.push(format!("{name}: unreadable record: {e}")),
        }
    }
}

/// The value at a dot path (`volumes.0.manifest`).
fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |cur, part| match cur {
        Value::Object(m) => m.get(part),
        Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Every way `resp` misses the record's `expect`.
fn check(expect: &Value, resp: &Resp) -> Vec<String> {
    let mut bad = Vec::new();
    let statuses: Vec<u64> = match &expect["status"] {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).collect(),
        Value::Number(n) => n.as_u64().into_iter().collect(),
        _ => Vec::new(),
    };
    if statuses.is_empty() {
        bad.push("expect.status is missing".into());
    } else if !statuses.contains(&u64::from(resp.status)) {
        bad.push(format!(
            "status {} not in {statuses:?}: {}",
            resp.status,
            resp.text().chars().take(300).collect::<String>()
        ));
    }
    let fields: Vec<&str> = expect["body_fields"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !fields.is_empty() {
        match resp.json() {
            Some(body) => {
                for f in fields {
                    if at(&body, f).is_none_or(Value::is_null) {
                        bad.push(format!("body field {f} is missing or null"));
                    }
                }
            }
            None => bad.push(format!(
                "body is not JSON, but the reader reads {fields:?} from it"
            )),
        }
    }
    let nullable: Vec<&str> = expect["nullable_fields"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let values = expect["body_values"].as_object();
    if !nullable.is_empty() || values.is_some() {
        match resp.json() {
            Some(body) => {
                for f in nullable {
                    if at(&body, f).is_none() {
                        bad.push(format!("body field {f} is missing (it may be null)"));
                    }
                }
                for (f, want) in values.into_iter().flatten() {
                    match at(&body, f) {
                        Some(got) if got == want => {}
                        got => bad.push(format!("body field {f} is {got:?}, not {want}")),
                    }
                }
            }
            None => bad.push("body is not JSON, but the reader reads fields from it".into()),
        }
    }
    for h in expect["headers"].as_array().into_iter().flatten() {
        if let Some(h) = h.as_str()
            && resp.header(&h.to_ascii_lowercase()).is_none()
        {
            bad.push(format!("header {h} is missing"));
        }
    }
    if let Some(prefixes) = expect["header_prefixes"].as_object() {
        for (h, want) in prefixes {
            let want = want.as_str().unwrap_or_default();
            let got = resp.header(&h.to_ascii_lowercase()).unwrap_or_default();
            if !got.starts_with(want) {
                bad.push(format!("header {h} is {got:?}, not {want}..."));
            }
        }
    }
    bad
}

/// The reader's origin: a browser sends it with every request the reader makes.
const READER_ORIGIN: &str = "https://reader.mokuro.app";

/// Request headers a browser sends without a preflight (the Fetch spec's CORS-safelisted
/// ones; `content-type` only with a form or text value).
fn safelisted_request_header(name: &str, value: &str) -> bool {
    match name {
        "accept" | "accept-language" | "content-language" => true,
        "content-type" => {
            let v = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            matches!(
                v.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        _ => false,
    }
}

/// Response headers a page can read without them being exposed.
fn safelisted_response_header(name: &str) -> bool {
    matches!(
        name,
        "cache-control"
            | "content-language"
            | "content-length"
            | "content-type"
            | "expires"
            | "last-modified"
            | "pragma"
    )
}

fn comma_list(resp: &Resp, name: &str) -> Vec<String> {
    resp.header(name)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The headers of `req` a browser would name in a preflight (`Authorization` included
/// when the record authenticates), lower-case.
fn preflight_headers(req: &Req) -> Vec<String> {
    let mut names: Vec<String> = req
        .headers
        .iter()
        .filter(|(k, v)| k != "origin" && !safelisted_request_header(k, v))
        .map(|(k, _)| k.clone())
        .collect();
    if req.auth != "none" {
        names.push("authorization".into());
    }
    names.sort();
    names.dedup();
    names
}

/// What a browser on the reader's origin does before `req`: a preflight when the method
/// or a header is not simple. Every way Bunko's answer would block the request.
async fn check_preflight(server: &Server, req: &Req) -> Vec<String> {
    let headers = preflight_headers(req);
    let simple_method = matches!(req.method.as_str(), "GET" | "HEAD" | "POST");
    if simple_method && headers.is_empty() {
        return Vec::new();
    }
    let mut pre = Req::new("OPTIONS", &req.path, "none")
        .header("origin", READER_ORIGIN)
        .header("access-control-request-method", &req.method);
    if !headers.is_empty() {
        pre = pre.header("access-control-request-headers", &headers.join(", "));
    }
    let resp = server.send(&pre).await;
    let mut bad = Vec::new();
    if resp.status != 204 && resp.status != 200 {
        bad.push(format!("preflight answered {}", resp.status));
    }
    if resp.header("access-control-allow-origin") != Some(READER_ORIGIN) {
        bad.push("preflight does not allow the reader's origin".into());
    }
    let methods = comma_list(&resp, "access-control-allow-methods");
    if !simple_method && !methods.contains(&req.method.to_ascii_lowercase()) {
        bad.push(format!("preflight does not allow {}", req.method));
    }
    let allowed = comma_list(&resp, "access-control-allow-headers");
    for h in headers {
        if !allowed.contains(&h) {
            bad.push(format!("preflight does not allow the {h} header"));
        }
    }
    bad
}

/// The answer as a page on the reader's origin sees it: allowed, and every header the
/// reader reads exposed.
fn check_cors(expect: &Value, resp: &Resp) -> Vec<String> {
    let mut bad = Vec::new();
    if resp.header("access-control-allow-origin") != Some(READER_ORIGIN) {
        bad.push("the answer does not allow the reader's origin".into());
        return bad;
    }
    let exposed = comma_list(resp, "access-control-expose-headers");
    for h in expect["headers"].as_array().into_iter().flatten() {
        if let Some(h) = h.as_str().map(str::to_ascii_lowercase)
            && !safelisted_response_header(&h)
            && !exposed.contains(&h)
        {
            bad.push(format!("header {h} is not exposed to the reader's page"));
        }
    }
    bad
}

/// The record's request with the ETag sentinel replaced by the one its `after` record
/// got back.
fn with_etag(req: &Req, etag: Option<&str>) -> Req {
    let mut req = req.clone();
    if let Some(etag) = etag {
        for (_, v) in req.headers.iter_mut() {
            if v == ETAG_SENTINEL {
                *v = etag.to_string();
            }
        }
    }
    req
}

/// Run `name` and then every record chained after it, on one server.
async fn run_chain(
    server: &Server,
    records: &[Record],
    index: usize,
    etag: Option<String>,
    failures: &mut Vec<String>,
    ran: &mut Vec<String>,
) {
    let r = &records[index];
    ran.push(r.name.clone());
    let mut req = with_etag(&r.req, etag.as_deref());
    if !req.headers.iter().any(|(k, _)| k == "origin") {
        req.headers.push(("origin".into(), READER_ORIGIN.into()));
    }
    let mut problems = check_preflight(server, &req).await;
    let resp = server.send(&req).await;
    problems.extend(check(&r.expect, &resp));
    problems.extend(check_cors(&r.expect, &resp));
    for problem in problems {
        failures.push(format!("{}: {problem}", r.name));
    }
    let got = resp.header("etag").map(str::to_string);
    for (i, next) in records.iter().enumerate() {
        if next.after.as_deref() == Some(r.name.as_str()) {
            Box::pin(run_chain(server, records, i, got.clone(), failures, ran)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_the_reader_requests() {
    let dir = contract_dir().join("reader");
    let mut records = Vec::new();
    let mut failures = Vec::new();
    load(&dir, "", &mut records, &mut failures);
    let recorded = records.len();
    load(&dir.join("legacy"), "legacy/", &mut records, &mut failures);
    if recorded == 0 {
        println!(
            "note: no reader records in {} yet (the reader writes them with READER_BLESS=1); only the legacy ones run",
            dir.display()
        );
    }
    let seed = Seed::load();
    let mut ran = Vec::new();
    for i in 0..records.len() {
        if records[i].after.is_some() {
            continue;
        }
        // Each chain on a fresh server, so no record sees another's writes.
        let server = Server::local(&seed).await;
        run_chain(&server, &records, i, None, &mut failures, &mut ran).await;
        server.stop().await;
    }
    for r in &records {
        if !ran.contains(&r.name) {
            failures.push(format!(
                "{}: runs after {:?}, which is not a record",
                r.name,
                r.after.as_deref().unwrap_or_default()
            ));
        }
    }
    println!("replayed {} reader record(s)", ran.len());
    assert!(
        failures.is_empty(),
        "{} reader contract failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
