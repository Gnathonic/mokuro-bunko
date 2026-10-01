//! The archive fetcher against a local HTTP server that misbehaves on request.

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::routing::get;
use bunko_processor::client::{Credentials, http_client};
use bunko_processor::config::TlsVerify;
use bunko_processor::fetch::{ArchiveFetcher, FetchError, FetchTiming};
use bunko_processor::spool::ArchiveSpool;
use bunko_processor::{CancelToken, tls};
use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::Value;

#[derive(Clone, Debug)]
enum Behave {
    Normal,
    /// Send the headers for the whole answer, then this many bytes, then drop.
    CutAfter(usize),
    /// Answer 200 with the whole file whatever the Range, cut after this many bytes.
    IgnoreRangeCutAfter(usize),
    Status(u16, Option<u64>),
    /// Say nothing at all for this long.
    StallHeaders(Duration),
    /// Flip the byte at this file offset in this answer only.
    CorruptAt(usize),
    /// Replace the file (and its ETag) before answering.
    Change(Vec<u8>, String),
    Accel,
}

struct Library {
    body: Vec<u8>,
    etag: String,
    script: VecDeque<Behave>,
    default: Behave,
    /// (Range, If-Range, Authorization) per request.
    requests: Vec<(Option<String>, Option<String>, Option<String>)>,
}

type Shared = Arc<Mutex<Library>>;

fn h(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn serve_file(State(lib): State<Shared>, headers: HeaderMap) -> Response<Body> {
    let (behave, body, etag) = {
        let mut lib = lib.lock();
        lib.requests.push((
            h(&headers, header::RANGE),
            h(&headers, header::IF_RANGE),
            h(&headers, header::AUTHORIZATION),
        ));
        let mut behave = lib
            .script
            .pop_front()
            .unwrap_or_else(|| lib.default.clone());
        if let Behave::Change(body, etag) = behave {
            lib.body = body;
            lib.etag = etag;
            behave = Behave::Normal;
        }
        (behave, lib.body.clone(), lib.etag.clone())
    };
    match behave {
        Behave::Status(code, retry) => {
            let mut r = Response::builder().status(code);
            if let Some(s) = retry {
                r = r.header(header::RETRY_AFTER, s.to_string());
            }
            return r.body(Body::from("no")).unwrap();
        }
        Behave::Accel => {
            return Response::builder()
                .header("X-Accel-Redirect", "/internal/x.cbz")
                .body(Body::empty())
                .unwrap();
        }
        Behave::StallHeaders(d) => tokio::time::sleep(d).await,
        _ => {}
    }
    let ignore_range = matches!(behave, Behave::IgnoreRangeCutAfter(_));
    let mut start = h(&headers, header::RANGE).and_then(|r| {
        r.strip_prefix("bytes=")
            .and_then(|r| r.strip_suffix('-'))
            .and_then(|n| n.parse::<usize>().ok())
    });
    if let Some(if_range) = h(&headers, header::IF_RANGE)
        && if_range != etag
    {
        start = None;
    }
    if ignore_range {
        start = None;
    }
    let from = start.unwrap_or(0).min(body.len());
    let mut data = body[from..].to_vec();
    if let Behave::CorruptAt(at) = behave
        && at >= from
        && at - from < data.len()
    {
        data[at - from] ^= 0xff;
    }
    let total = body.len();
    let mut response = Response::builder()
        .header(header::ETAG, etag.as_str())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, data.len().to_string());
    if start.is_some() {
        response = response.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {from}-{}/{total}", total - 1),
        );
    }
    let cut = match behave {
        Behave::CutAfter(n) | Behave::IgnoreRangeCutAfter(n) => Some(n.min(data.len())),
        _ => None,
    };
    let body = match cut {
        Some(n) => {
            let first = Bytes::from(data[..n].to_vec());
            // The bytes go out first; the cut comes a moment later (an error at once
            // would abort the connection before anything was flushed).
            let stream = futures_util::stream::iter(vec![Ok::<Bytes, std::io::Error>(first)])
                .chain(futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Err(std::io::Error::other("cut"))
                }));
            Body::from_stream(stream)
        }
        None => {
            let chunks: Vec<Result<Bytes, std::io::Error>> = data
                .chunks(16 * 1024)
                .map(|c| Ok(Bytes::from(c.to_vec())))
                .collect();
            Body::from_stream(futures_util::stream::iter(chunks))
        }
    };
    response.body(body).unwrap()
}

async fn library(body: Vec<u8>, script: Vec<Behave>, default: Behave) -> (String, Shared) {
    let lib = Arc::new(Mutex::new(Library {
        body,
        etag: "\"v1\"".into(),
        script: script.into(),
        default,
        requests: Vec::new(),
    }));
    let app = Router::new()
        .route("/mokuro-reader/{*path}", get(serve_file))
        .with_state(lib.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), lib)
}

fn timing() -> FetchTiming {
    FetchTiming {
        connect_timeout: Duration::from_millis(300),
        read_timeout: Duration::from_millis(300),
        retry_delays: vec![
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(40),
        ],
        stall: Duration::from_secs(1),
        max_restarts: 3,
        retry_after_cap: Duration::from_millis(50),
        progress_after: Duration::ZERO,
        progress_every: Duration::ZERO,
    }
}

fn fetcher(base: &str, storage: &std::path::Path, memory_mb: u64) -> ArchiveFetcher {
    let tls = tls::client_config(&TlsVerify::Yes).unwrap();
    let http = http_client(&tls, Duration::from_secs(2), false).unwrap();
    let creds = Arc::new(Credentials::new("gpu", "pw"));
    let spool = Arc::new(ArchiveSpool::new(storage, memory_mb));
    ArchiveFetcher::new(http, base, creds, spool, timing())
}

const PATH: &str = "/mokuro-reader/Series A/Vol 1.cbz";

fn archive() -> Vec<u8> {
    common::volume(4, 50_000)
}

fn crc(bytes: &[u8]) -> String {
    format!("{:08x}", crc32fast::hash(bytes))
}

type Seen = Arc<Mutex<Vec<BTreeMap<String, Value>>>>;

async fn fetch(
    f: &ArchiveFetcher,
    size: Option<u64>,
    seen: &Seen,
) -> Result<bunko_processor::fetch::FetchedArchive, FetchError> {
    let cancel = CancelToken::new();
    let sink = seen.clone();
    let progress = move |d: BTreeMap<String, Value>| sink.lock().push(d);
    f.fetch(PATH, size, &cancel, Some(&progress), "test").await
}

fn fault(
    result: Result<bunko_processor::fetch::FetchedArchive, FetchError>,
) -> bunko_processor::fetch::TransferFault {
    match result {
        Err(FetchError::Fault(f)) => f,
        other => panic!("expected a fault, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_download_is_verified_and_summarised() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, lib) = library(body.clone(), vec![], Behave::Normal).await;
    let seen = Seen::default();
    let got = fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(body.len() as u64),
        &seen,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(got.path()).unwrap(), body);
    let s = got.summary();
    assert_eq!(s["bytes"], body.len());
    assert_eq!(s["crc32"], crc(&body));
    assert_eq!(s["requests"], 1);
    assert_eq!(s["members"], 4);
    assert_eq!(s["restarts"], 0);
    assert!(!s.contains_key("verdict"));
    let requests = lib.lock().requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].2.as_deref(),
        Some("Basic Z3B1OnB3"),
        "the fetch carries the credentials"
    );
    assert!(seen.lock().iter().any(|d| d["state"] == "downloading"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_download_resumes_with_range_and_if_range() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, lib) = library(
        body.clone(),
        vec![Behave::CutAfter(30_000), Behave::CutAfter(40_000)],
        Behave::Normal,
    )
    .await;
    let seen = Seen::default();
    let got = fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(body.len() as u64),
        &seen,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(got.path()).unwrap(), body);
    assert_eq!((got.requests, got.restarts), (3, 0));
    let requests = lib.lock().requests.clone();
    assert_eq!(requests[0].0, None);
    assert_eq!(requests[1].0.as_deref(), Some("bytes=30000-"));
    assert_eq!(requests[1].1.as_deref(), Some("\"v1\""));
    assert_eq!(requests[2].0.as_deref(), Some("bytes=70000-"));
    let retrying: Vec<_> = seen
        .lock()
        .iter()
        .filter(|d| d["state"] == "retrying")
        .cloned()
        .collect();
    assert_eq!(retrying.len(), 2);
    assert_eq!(retrying[0]["bytes"], 30_000);
    assert!(retrying[0]["retry_in"].is_number());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_replaced_mid_download_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let old = archive();
    let new = common::volume(3, 40_000);
    let (base, lib) = library(
        old,
        vec![
            Behave::CutAfter(30_000),
            Behave::Change(new.clone(), "\"v2\"".into()),
        ],
        Behave::Normal,
    )
    .await;
    let seen = Seen::default();
    let got = fetch(&fetcher(&base, dir.path(), 0), None, &seen)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(got.path()).unwrap(),
        new,
        "never a splice of two files"
    );
    assert_eq!((got.requests, got.restarts), (2, 1));
    assert_eq!(lib.lock().requests[1].1.as_deref(), Some("\"v1\""));
    assert!(seen.lock().iter().any(|d| d["state"] == "restarting"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proxy_that_ignores_range_is_no_range() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, _lib) = library(body.clone(), vec![], Behave::IgnoreRangeCutAfter(30_000)).await;
    let f = fault(
        fetch(
            &fetcher(&base, dir.path(), 2048),
            Some(body.len() as u64),
            &Seen::default(),
        )
        .await,
    );
    assert_eq!(f.kind, "no_range", "{f:?}");
    assert_eq!(f.requests, 5);
    assert_eq!(f.detail()["bytes"], 30_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn silence_is_a_stall() {
    let dir = tempfile::tempdir().unwrap();
    let (base, lib) = library(
        archive(),
        vec![],
        Behave::StallHeaders(Duration::from_secs(5)),
    )
    .await;
    let began = std::time::Instant::now();
    let f = fault(fetch(&fetcher(&base, dir.path(), 2048), None, &Seen::default()).await);
    assert_eq!(f.kind, "stalled", "{f:?}");
    assert!(
        f.message.starts_with("no new byte for 1 s at byte 0 of ?"),
        "{}",
        f.message
    );
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(lib.lock().requests.len() >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn statuses_map_to_classes() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let len = body.len() as u64;
    let cases: Vec<(Vec<Behave>, &str)> = vec![
        (vec![Behave::Status(404, None)], "missing"),
        (vec![Behave::Status(410, None)], "missing"),
        (vec![Behave::Status(418, None)], "rejected"),
        (
            vec![Behave::Status(500, None), Behave::Status(500, None)],
            "stalled",
        ),
        (vec![Behave::Accel], "mismatch"),
    ];
    for (script, class) in cases {
        let (base, _lib) = library(body.clone(), script.clone(), Behave::Normal).await;
        let f = fault(
            fetch(
                &fetcher(&base, dir.path(), 2048),
                Some(len),
                &Seen::default(),
            )
            .await,
        );
        assert_eq!(f.kind, class, "{script:?}: {f:?}");
        if class == "stalled" {
            assert_eq!(f.status, Some(500));
            assert!(
                f.message
                    .contains("answered 500 twice in a row for Series A/Vol 1.cbz"),
                "{}",
                f.message
            );
        }
    }
    // The size the library promised is checked.
    let (base, _lib) = library(body.clone(), vec![], Behave::Normal).await;
    let f = fault(
        fetch(
            &fetcher(&base, dir.path(), 2048),
            Some(len + 1),
            &Seen::default(),
        )
        .await,
    );
    assert_eq!(f.kind, "mismatch");
    // Transient answers are retried (Retry-After honoured, capped).
    let (base, lib) = library(
        body.clone(),
        vec![
            Behave::Status(503, Some(30)),
            Behave::Status(429, None),
            Behave::Status(502, None),
        ],
        Behave::Normal,
    )
    .await;
    let got = fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(len),
        &Seen::default(),
    )
    .await
    .unwrap();
    assert_eq!(got.requests, 4);
    assert_eq!(lib.lock().requests.len(), 4);
    // A refused account steps the whole processor away.
    let (base, _lib) = library(body, vec![Behave::Status(401, None)], Behave::Normal).await;
    match fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(len),
        &Seen::default(),
    )
    .await
    {
        Err(FetchError::LostLibrary(m)) => assert!(m.contains("401"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn corruption_in_transit_is_repaired_by_a_second_copy() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, lib) = library(body.clone(), vec![Behave::CorruptAt(1_000)], Behave::Normal).await;
    let got = fetch(
        &fetcher(&base, dir.path(), 0),
        Some(body.len() as u64),
        &Seen::default(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(got.path()).unwrap(), body);
    assert_eq!(got.repairs, 1);
    assert_eq!(got.verdict, None);
    assert_eq!(got.damaged_note(), None);
    assert_eq!(lib.lock().requests.len(), 2);
    // Only the copy handed over is still on disk.
    let held: Vec<_> = std::fs::read_dir(dir.path().join(".processing/archives"))
        .unwrap()
        .collect();
    assert_eq!(held.len(), 1);
    let path = got.path().to_path_buf();
    drop(got);
    assert!(!path.exists(), "dropping the archive releases its file");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_bad_bytes_twice_are_damaged_at_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = archive();
    body[1_000] ^= 0xff;
    let (base, _lib) = library(body.clone(), vec![], Behave::Normal).await;
    let got = fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(body.len() as u64),
        &Seen::default(),
    )
    .await
    .unwrap();
    assert_eq!(got.verdict.as_deref(), Some("damaged at the library"));
    assert_eq!(got.damaged, vec!["001.jpg".to_string()]);
    let s = got.summary();
    assert_eq!(s["verdict"], "damaged at the library");
    assert_eq!(s["damaged"], serde_json::json!(["001.jpg"]));
    assert_eq!(
        got.damaged_note().unwrap(),
        " (the library's copy: the same bytes on two downloads; '001.jpg' fails its CRC-32 check)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_different_bad_copies_differ() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, _lib) = library(
        body.clone(),
        vec![Behave::CorruptAt(1_000), Behave::CorruptAt(2_000)],
        Behave::Normal,
    )
    .await;
    let f = fault(
        fetch(
            &fetcher(&base, dir.path(), 2048),
            Some(body.len() as u64),
            &Seen::default(),
        )
        .await,
    );
    assert_eq!(f.kind, "differs");
    assert!(
        f.message
            .starts_with("two downloads failed their CRC checks with different bytes"),
        "{}",
        f.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_ends_a_download_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _lib) = library(
        archive(),
        vec![],
        Behave::StallHeaders(Duration::from_secs(5)),
    )
    .await;
    let f = fetcher(&base, dir.path(), 2048);
    let cancel = CancelToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
    });
    let began = std::time::Instant::now();
    let result = f.fetch(PATH, None, &cancel, None, "test").await;
    assert!(matches!(result, Err(FetchError::Cancelled)), "{result:?}");
    assert!(began.elapsed() < Duration::from_millis(500));
}

#[tokio::test(flavor = "multi_thread")]
async fn placement_follows_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let body = archive();
    let (base, _lib) = library(body.clone(), vec![], Behave::Normal).await;
    let disk = fetch(
        &fetcher(&base, dir.path(), 0),
        Some(body.len() as u64),
        &Seen::default(),
    )
    .await
    .unwrap();
    assert_eq!(disk.summary()["placement"], "disk");
    assert!(
        disk.path()
            .starts_with(dir.path().join(".processing/archives"))
    );
    let ram = fetch(
        &fetcher(&base, dir.path(), 2048),
        Some(body.len() as u64),
        &Seen::default(),
    )
    .await
    .unwrap();
    if cfg!(target_os = "linux")
        && bunko_processor::spool::memory_headroom().is_none_or(|h| h > 2 << 30)
    {
        assert_eq!(ram.summary()["placement"], "memory");
        assert_eq!(std::fs::read(ram.path()).unwrap(), body);
    }
}
