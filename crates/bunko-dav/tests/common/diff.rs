//! The differential harness shared by `bunko-dav/tests/differential.rs` (Python 0.5.2 vs
//! `Dav::handle`) and `bunko-server/tests/differential_053.rs` (Python 0.5.3 vs the whole
//! Rust server): a request script, the Python reference server's process, and the
//! normalisation both answers go through before they are compared. Included by path from
//! the server's tests, so it depends only on crates both test targets have.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

use http::{HeaderMap, StatusCode};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

/// A fixed mtime with sub-microsecond digits, so ETag float formatting is exercised.
pub const MTIME_SECS: u64 = 1_790_878_208;
pub const MTIME_NANOS: u32 = 881_310_123;

/// One side's raw answer.
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn set_mtime(path: &Path) {
    let t = UNIX_EPOCH + Duration::new(MTIME_SECS, MTIME_NANOS);
    let f = std::fs::File::open(path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t))
        .unwrap();
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub struct Step {
    pub name: &'static str,
    pub user: Option<&'static str>,
    pub method: &'static str,
    pub path: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    /// Response header to remember (per side) as `{var}` for later steps.
    pub capture: Vec<(&'static str, &'static str)>,
    /// Compare ETag/Last-Modified values exactly (only before any write).
    pub exact_times: bool,
    /// The intended difference, if this step is one of the fixes.
    pub intended: Option<&'static str>,
}

pub fn step(
    name: &'static str,
    user: Option<&'static str>,
    method: &'static str,
    path: &'static str,
) -> Step {
    Step {
        name,
        user,
        method,
        path,
        headers: Vec::new(),
        body: Vec::new(),
        capture: Vec::new(),
        exact_times: false,
        intended: None,
    }
}

impl Step {
    pub fn h(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    pub fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
    pub fn exact(mut self) -> Self {
        self.exact_times = true;
        self
    }
    pub fn capture(mut self, header: &'static str, var: &'static str) -> Self {
        self.capture.push((header, var));
        self
    }
    pub fn intended(mut self, why: &'static str) -> Self {
        self.intended = Some(why);
        self
    }
}

#[derive(Debug, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// Headers clients act on (value-normalised where the two sides legitimately differ).
pub const COMPARED: [&str; 14] = [
    "content-type",
    "allow",
    "dav",
    "ms-author-via",
    "accept-ranges",
    "content-range",
    "x-mokuro-upload",
    "x-mokuro-size",
    "x-mokuro-put",
    "x-mokuro-digest-verified",
    "x-accel-redirect",
    "lock-token",
    "etag",
    "content-length",
];

pub fn normalise(r: &Resp, exact: bool, method: &str) -> Answer {
    let mut headers = BTreeMap::new();
    for name in COMPARED {
        if let Some(v) = r.header(name) {
            let v = match name {
                "content-type" => v.to_ascii_lowercase().replace("; ", ";"),
                "lock-token" => "<present>".to_string(),
                "etag" if !exact => normalise_etag(v.trim_matches('"')),
                "content-length" if r.status.as_u16() >= 400 && !v.is_empty() => "<n>".to_string(),
                // XML/HTML documents: their bytes are compared normalised (or not at all).
                "content-length"
                    if r.header("content-type")
                        .is_some_and(|t| t.contains("xml") || t.contains("html")) =>
                {
                    "<n>".to_string()
                }
                _ => v.to_string(),
            };
            headers.insert(name.to_string(), v);
        }
    }
    let ctype = r.header("content-type").unwrap_or("");
    let body = if ctype.contains("xml") {
        canonical_xml(&r.body, exact)
    } else if ctype.contains("json") {
        serde_json::from_slice::<serde_json::Value>(&r.body)
            .map(|v| v.to_string())
            .unwrap_or_else(|_| r.text())
    } else if r.status.is_success() && (method == "GET" || method == "HEAD") {
        format!("{} bytes, sha {:x}", r.body.len(), {
            use sha2::Digest;
            sha2::Sha256::digest(&r.body)
        })
    } else {
        String::new() // status pages: clients read only the status
    };
    Answer {
        status: r.status.as_u16(),
        headers,
        body,
    }
}

/// `1790878208.881310-123` -> `<mtime>-123`.
fn normalise_etag(v: &str) -> String {
    let (time, size) = match v.split_once('-') {
        Some((t, s)) => (t, Some(s)),
        None => (v, None),
    };
    let ok = time.split_once('.').is_some_and(|(a, b)| {
        a.chars().all(|c| c.is_ascii_digit())
            && b.len() == 6
            && b.chars().all(|c| c.is_ascii_digit())
    });
    let t = if ok { "<mtime>" } else { "<BAD-ETAG>" };
    match size {
        Some(s) => format!("{t}-{s}"),
        None => t.to_string(),
    }
}

#[derive(Debug)]
struct El {
    name: String,
    text: String,
    children: Vec<El>,
}

fn parse_xml(body: &[u8]) -> Option<El> {
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<El> = Vec::new();
    let mut root = None;
    loop {
        let (ns, ev) = reader.read_resolved_event_into(&mut buf).ok()?;
        let name_of = |local: &[u8]| {
            let ns = match &ns {
                ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
                _ => String::new(),
            };
            format!("{{{ns}}}{}", String::from_utf8_lossy(local))
        };
        match ev {
            Event::Start(s) => stack.push(El {
                name: name_of(s.local_name().as_ref()),
                text: String::new(),
                children: vec![],
            }),
            Event::Empty(s) => {
                let el = El {
                    name: name_of(s.local_name().as_ref()),
                    text: String::new(),
                    children: vec![],
                };
                match stack.last_mut() {
                    Some(p) => p.children.push(el),
                    None => root = Some(el),
                }
            }
            Event::End(_) => {
                let el = stack.pop()?;
                match stack.last_mut() {
                    Some(p) => p.children.push(el),
                    None => root = Some(el),
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.unescape().ok()?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    root
}

fn canon(el: &El, exact: bool, href: &str) -> String {
    let local = el.name.rsplit('}').next().unwrap_or("");
    let virtual_root = href == "/" || href == "/mokuro-reader/";
    let text = match local {
        "creationdate" => "<date>".to_string(),
        "getlastmodified" if !exact || virtual_root => "<date>".to_string(),
        "getetag" if !exact => normalise_etag(&el.text),
        "timeout" => "<timeout>".to_string(),
        _ if el.text.starts_with("opaquelocktoken:") => "<token>".to_string(),
        _ => el.text.clone(),
    };
    let href_here = el
        .children
        .iter()
        .find(|c| c.name == "{DAV:}href")
        .map(|c| c.text.as_str())
        .unwrap_or(href);
    let mut kids: Vec<String> = el
        .children
        .iter()
        // Compiled metadata (`<Series>/series.json`, root `catalog.json`) is written by the
        // Python stack's background compiler, not by DAV: never part of the comparison.
        .filter(|c| !(c.name == "{DAV:}response" && is_compiled_href(c)))
        .map(|c| canon(c, exact, href_here))
        .collect();
    kids.sort();
    format!("<{}>{}{}</>", el.name, text, kids.concat())
}

fn is_compiled_href(response: &El) -> bool {
    response
        .children
        .iter()
        .find(|c| c.name == "{DAV:}href")
        .and_then(|h| {
            percent_encoding::percent_decode_str(&h.text)
                .decode_utf8()
                .ok()
        })
        .is_some_and(|p| bunko_dav::is_compiled_metadata_path(&p))
}

pub fn canonical_xml(body: &[u8], exact: bool) -> String {
    match parse_xml(body) {
        Some(root) => canon(&root, exact, ""),
        None => format!("<unparsable: {}>", String::from_utf8_lossy(body)),
    }
}

pub fn substitute(v: &str, vars: &HashMap<String, String>) -> String {
    let mut out = v.to_string();
    for (k, val) in vars {
        out = out.replace(&format!("{{{k}}}"), val);
    }
    out
}

/// An HTTP client that leaves both answers as sent (no gzip, proxy or redirects).
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_gzip()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Start `ref052_server.py` (it serves whichever `mokuro_bunko` the interpreter imports)
/// over `base` on `port`, and wait until it answers. `None` when it never came up.
pub async fn start_reference(
    python: &Path,
    base: &Path,
    port: u16,
    client: &reqwest::Client,
) -> Option<Server> {
    let dav_tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bunko-dav/tests");
    let server = Server(
        Command::new(python)
            .arg(dav_tests.join("golden/ref052_server.py"))
            .arg(base)
            .arg(port.to_string())
            .current_dir(base)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the Python reference server"),
    );
    let origin = format!("http://127.0.0.1:{port}");
    for _ in 0..120 {
        if client
            .get(format!("{origin}/api/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Some(server);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

/// Send one step to the Python reference over HTTP (Basic `pass1234`).
pub async fn python_send(
    client: &reqwest::Client,
    origin: &str,
    s: &Step,
    vars: &HashMap<String, String>,
) -> Resp {
    let method = reqwest::Method::from_bytes(s.method.as_bytes()).unwrap();
    let mut rb = client.request(method, format!("{origin}{}", s.path));
    if let Some(u) = s.user {
        rb = rb.basic_auth(u, Some("pass1234"));
    }
    for (k, v) in &s.headers {
        rb = rb.header(*k, substitute(v, vars));
    }
    let pr = rb.body(s.body.clone()).send().await.unwrap();
    let status = pr.status();
    let headers = pr.headers().clone();
    let body = pr.bytes().await.unwrap().to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

/// The comparison of a whole script: every difference must be one of the intended ones.
#[derive(Default)]
pub struct Tally {
    pub unexpected: Vec<&'static str>,
    pub intended_seen: Vec<String>,
    pub report: bool,
}

impl Tally {
    pub fn new() -> Self {
        Tally {
            report: std::env::var_os("BUNKO_DAV_DIFF_REPORT").is_some(),
            ..Default::default()
        }
    }

    /// Remember the step's captured headers per side, then compare the two answers.
    pub fn step(
        &mut self,
        s: &Step,
        py: &Resp,
        rs: &Resp,
        py_vars: &mut HashMap<String, String>,
        rs_vars: &mut HashMap<String, String>,
    ) {
        for (header, var) in s.capture.iter().copied() {
            for (resp, vars) in [(py, &mut *py_vars), (rs, &mut *rs_vars)] {
                if let Some(v) = resp.header(header) {
                    vars.insert(
                        var.to_string(),
                        v.trim_matches(|c| c == '<' || c == '>').to_string(),
                    );
                }
            }
        }
        let (a, b) = (
            normalise(py, s.exact_times, s.method),
            normalise(rs, s.exact_times, s.method),
        );
        let same = a == b;
        if self.report || !same {
            eprintln!(
                "== {} ({} {}): python {} / rust {}{}",
                s.name,
                s.method,
                s.path,
                a.status,
                b.status,
                if same { "" } else { "  DIFFERENT" }
            );
            if !same {
                if a.headers != b.headers {
                    eprintln!(
                        "   python headers {:?}\n   rust   headers {:?}",
                        a.headers, b.headers
                    );
                }
                if a.body != b.body {
                    eprintln!("   python body {}\n   rust   body {}", a.body, b.body);
                }
            }
        }
        match (same, s.intended) {
            (false, None) => self.unexpected.push(s.name),
            (false, Some(why)) => self.intended_seen.push(format!("{}: {why}", s.name)),
            (true, Some(why)) => {
                eprintln!("   (intended difference did not show: {} -- {why})", s.name)
            }
            (true, None) => {}
        }
    }

    /// Print the intended differences and fail on any other.
    pub fn finish(self, against: &str) {
        eprintln!("intended differences ({}):", self.intended_seen.len());
        for d in &self.intended_seen {
            eprintln!("  - {d}");
        }
        assert!(
            self.unexpected.is_empty(),
            "unexpected differences from {against}: {:?}",
            self.unexpected
        );
    }
}
