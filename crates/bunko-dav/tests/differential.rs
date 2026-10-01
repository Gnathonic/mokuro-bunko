//! Differential test: the same request script against the Python 0.5.2 server (over HTTP)
//! and against `Dav::handle` (in process), each over its own copy of one storage tree.
//! Statuses, the headers clients read, and normalised XML/JSON bodies are compared; every
//! difference must be one of the intended fixes listed in the script.
//!
//! Needs `~/.cache/mokuro-bunko-demo/ref052/bin/python` (the 0.5.2 reference env, see
//! CONVENTIONS.md); skipped with a message when it is absent. `BUNKO_DAV_DIFF_REPORT=1`
//! prints every step.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use bunko_core::Role;
use bunko_dav::{DavContext, DavHooks, NoHooks};
use common::{Resp, cbz_bytes, damaged_cbz, digest_header};
use http::Request;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

const PYTHON: &str = ".cache/mokuro-bunko-demo/ref052/bin/python";
/// A fixed mtime with sub-microsecond digits, so ETag float formatting is exercised.
const MTIME_SECS: u64 = 1_790_878_208;
const MTIME_NANOS: u32 = 881_310_123;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn set_mtime(path: &Path) {
    let t = UNIX_EPOCH + Duration::new(MTIME_SECS, MTIME_NANOS);
    let f = std::fs::File::open(path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t))
        .unwrap();
}

/// The storage fixture (both trees are built by this, with identical mtimes).
fn build_tree(base: &Path) {
    let lib = base.join("library");
    for d in [
        "library/thumbnails",
        "library/series",
        "library/Series Ω",
        "library/Empty",
        "inbox",
        "users/reader",
    ] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("library/manga1.cbz", b"fake cbz content 1".to_vec()),
        ("library/manga2.cbz", b"fake cbz content 2".to_vec()),
        ("library/series/vol1.cbz", cbz_bytes(2)),
        ("library/series/vol1.mokuro", b"{}".to_vec()),
        ("library/series/vol1.webp", b"RIFF".to_vec()),
        ("library/series/notes.txt", b"0123456789".to_vec()),
        ("library/Series Ω/Vol #1.cbz", cbz_bytes(1)),
        (
            "library/Series Ω/Vol #1.hayai-nova.mokuro.gz",
            b"gz".to_vec(),
        ),
        ("users/reader/volume-data.json", b"reader progress".to_vec()),
    ];
    for (rel, data) in &files {
        std::fs::write(base.join(rel), data).unwrap();
        set_mtime(&base.join(rel));
    }
    for d in ["thumbnails", "series", "Series Ω", "Empty"] {
        set_mtime(&lib.join(d));
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Step {
    name: &'static str,
    user: Option<&'static str>,
    method: &'static str,
    path: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// Response header to remember (per side) as `{var}` for later steps.
    capture: Vec<(&'static str, &'static str)>,
    /// Compare ETag/Last-Modified values exactly (only before any write).
    exact_times: bool,
    /// The intended difference, if this step is one of the fixes.
    intended: Option<&'static str>,
}

fn step(
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
    fn h(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
    fn exact(mut self) -> Self {
        self.exact_times = true;
        self
    }
    fn capture(mut self, header: &'static str, var: &'static str) -> Self {
        self.capture.push((header, var));
        self
    }
    fn intended(mut self, why: &'static str) -> Self {
        self.intended = Some(why);
        self
    }
}

fn script() -> Vec<Step> {
    let named = r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:" xmlns:x="urn:x"><D:prop><D:getetag/><D:getcontentlength/><D:displayname/><x:foo/></D:prop></D:propfind>"#;
    let propname = r#"<D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#;
    let lockinfo = r#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>me</D:owner></D:lockinfo>"#;
    let proppatch = r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes></D:prop></D:set></D:propertyupdate>"#;
    let named_dead = r#"<D:propfind xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:prop><Z:Win32FileAttributes/><D:getcontentlength/></D:prop></D:propfind>"#;
    let good = cbz_bytes(3);
    let mut digest_bad = good.clone();
    digest_bad.push(0);
    vec![
        // --- reads, before any write: validators compared exactly ------------------
        step("propfind root d1", None, "PROPFIND", "/").h("depth", "1").exact(),
        step("propfind reader d1 anon", None, "PROPFIND", "/mokuro-reader").h("depth", "1").exact(),
        step("propfind reader d1 reader", Some("reader"), "PROPFIND", "/mokuro-reader/").h("depth", "1").exact(),
        step("propfind series d1", None, "PROPFIND", "/mokuro-reader/series").h("depth", "1").exact(),
        step("propfind unicode folder", None, "PROPFIND", "/mokuro-reader/Series%20%CE%A9/").h("depth", "1").exact(),
        step("propfind file d0", None, "PROPFIND", "/mokuro-reader/series/vol1.cbz").h("depth", "0").exact(),
        step("propfind progress d0", Some("reader"), "PROPFIND", "/mokuro-reader/volume-data.json").h("depth", "0").exact(),
        step("propfind progress missing", Some("uploader"), "PROPFIND", "/mokuro-reader/volume-data.json").h("depth", "0"),
        step("propfind infinity anon", None, "PROPFIND", "/mokuro-reader").h("depth", "infinity").exact(),
        step("propfind infinity reader", Some("reader"), "PROPFIND", "/mokuro-reader").h("depth", "infinity").exact(),
        step("propfind root infinity reader", Some("reader"), "PROPFIND", "/").h("depth", "infinity").exact(),
        step("propfind named", None, "PROPFIND", "/mokuro-reader/series").h("depth", "1").body(named).exact(),
        step("propfind propname", None, "PROPFIND", "/mokuro-reader/series/vol1.cbz").h("depth", "0").body(propname)
            .intended("RFC <propname/> is implemented (0.5.2 answered responses with no propstat)"),
        step("propfind bad depth", None, "PROPFIND", "/mokuro-reader").h("depth", "2"),
        step("propfind bad body", None, "PROPFIND", "/mokuro-reader").h("depth", "1").body("<nope"),
        step("propfind inbox", None, "PROPFIND", "/inbox").h("depth", "1"),
        step("propfind unknown", None, "PROPFIND", "/nope").h("depth", "0"),
        step("options root", None, "OPTIONS", "/"),
        step("options reader", None, "OPTIONS", "/mokuro-reader/"),
        step("options file", None, "OPTIONS", "/mokuro-reader/manga1.cbz"),
        step("options unmapped", None, "OPTIONS", "/mokuro-reader/series/nope.cbz"),
        step("options inbox", None, "OPTIONS", "/inbox"),
        step("options 404", None, "OPTIONS", "/foo/bar"),
        step("get file", None, "GET", "/mokuro-reader/manga1.cbz").exact().capture("etag", "etag1").capture("last-modified", "lm1"),
        step("get unicode file", None, "GET", "/mokuro-reader/Series%20%CE%A9/Vol%20%231.cbz").exact(),
        step("head file", None, "HEAD", "/mokuro-reader/manga1.cbz").exact(),
        step("get progress", Some("reader"), "GET", "/mokuro-reader/volume-data.json").exact(),
        step("get range", None, "GET", "/mokuro-reader/series/notes.txt").h("range", "bytes=2-4").exact(),
        step("get suffix range", None, "GET", "/mokuro-reader/series/notes.txt").h("range", "bytes=-3").exact(),
        step("get open range", None, "GET", "/mokuro-reader/series/notes.txt").h("range", "bytes=7-").exact(),
        step("get multi range", None, "GET", "/mokuro-reader/series/notes.txt").h("range", "bytes=0-0,5-9").exact()
            .intended("several ranges are coalesced into one spanning range (0.5.2 served only the highest one)"),
        step("get 416", None, "GET", "/mokuro-reader/series/notes.txt").h("range", "bytes=10-"),
        step("get collection", None, "GET", "/mokuro-reader/series/"),
        step("get 404", None, "GET", "/mokuro-reader/nope.cbz"),
        step("get if-none-match", None, "GET", "/mokuro-reader/manga1.cbz").h("if-none-match", "{etag1}")
            .intended("a 304 carries ETag and Last-Modified (RFC 9110 15.4.5; 0.5.2 sent none)"),
        step("get if-match fails", None, "GET", "/mokuro-reader/manga1.cbz").h("if-match", "\"nope\""),
        step("get if-modified-since equal", None, "GET", "/mokuro-reader/manga1.cbz").h("if-modified-since", "{lm1}")
            .intended("an equal If-Modified-Since date is 304 (0.5.2: 200, spec 14.5)"),
        step("get if-modified-since old", None, "GET", "/mokuro-reader/manga1.cbz").h("if-modified-since", "Thu, 01 Jan 1998 00:00:00 GMT"),
        step("post 405", None, "POST", "/mokuro-reader/").intended("405 lists Allow (0.5.2 sent none, spec 14.11)"),
        // --- writes ---------------------------------------------------------------
        step("put progress", Some("reader"), "PUT", "/mokuro-reader/volume-data.json").body("new progress")
            .intended("the ETag of an overwrite is the new file's (0.5.2 returned the old one, spec 14.4)"),
        step("put progress new user", Some("uploader"), "PUT", "/mokuro-reader/profiles.json").body("{}"),
        step("put cbz new", Some("uploader"), "PUT", "/mokuro-reader/series/new.cbz").body(good.clone()).capture("etag", "put_etag"),
        step("put cbz replace", Some("admin"), "PUT", "/mokuro-reader/series/new.cbz").body(cbz_bytes(4)).capture("etag", "replace_etag")
            .intended("the ETag of an overwrite is the new file's (0.5.2 returned the old one, spec 14.4)"),
        step("get after replace", None, "GET", "/mokuro-reader/series/new.cbz").capture("etag", "get_etag"),
        step("put not a zip", Some("uploader"), "PUT", "/mokuro-reader/series/bad.cbz").body("<html>login</html>".repeat(10)),
        step("put damaged", Some("uploader"), "PUT", "/mokuro-reader/series/dmg.cbz").body(damaged_cbz()),
        step("put damaged again", Some("uploader"), "PUT", "/mokuro-reader/series/dmg.cbz").body(damaged_cbz()),
        step("put digest mismatch", Some("uploader"), "PUT", "/mokuro-reader/series/dg.cbz").h("content-digest", digest_header(&good, "sha-256")).body(digest_bad),
        step("put digest ok", Some("uploader"), "PUT", "/mokuro-reader/series/dg.cbz").h("content-digest", digest_header(&good, "sha-512")).body(good.clone()),
        step("put sidecar", Some("admin"), "PUT", "/mokuro-reader/series/new.mokuro").body("{\"pages\": []}"),
        step("put no parent", Some("admin"), "PUT", "/mokuro-reader/NoSuch/x.cbz").body(good.clone()),
        step("put onto collection", Some("admin"), "PUT", "/mokuro-reader/series").body("x"),
        step("put if-none-match star", Some("admin"), "PUT", "/mokuro-reader/manga2.cbz").h("if-none-match", "*").body(good.clone()),
        step("propfind series after puts", Some("reader"), "PROPFIND", "/mokuro-reader/series").h("depth", "1"),
        step("propfind infinity after puts", Some("reader"), "PROPFIND", "/mokuro-reader").h("depth", "infinity")
            .intended("0.5.2 refreshes its cache before the write and may serve the pre-write listing; invalidated after commit here"),
        step("mkcol", Some("uploader"), "MKCOL", "/mokuro-reader/NewSeries"),
        step("mkcol exists", Some("uploader"), "MKCOL", "/mokuro-reader/NewSeries"),
        step("mkcol no parent", Some("uploader"), "MKCOL", "/mokuro-reader/a/b"),
        step("copy file", Some("admin"), "COPY", "/mokuro-reader/manga1.cbz").h("destination", "{host}/mokuro-reader/NewSeries/copy.cbz"),
        step("copy file overwrite F", Some("admin"), "COPY", "/mokuro-reader/manga1.cbz").h("destination", "{host}/mokuro-reader/NewSeries/copy.cbz").h("overwrite", "F"),
        step("move file", Some("admin"), "MOVE", "/mokuro-reader/manga2.cbz").h("destination", "{host}/mokuro-reader/NewSeries/moved.cbz"),
        step("move folder", Some("admin"), "MOVE", "/mokuro-reader/NewSeries").h("destination", "{host}/mokuro-reader/Renamed").h("depth", "infinity"),
        step("copy folder", Some("admin"), "COPY", "/mokuro-reader/Renamed").h("destination", "{host}/mokuro-reader/Copied"),
        step("move onto itself", Some("admin"), "MOVE", "/mokuro-reader/Copied").h("destination", "{host}/mokuro-reader/Copied"),
        step("move below itself", Some("admin"), "MOVE", "/mokuro-reader/Copied").h("destination", "{host}/mokuro-reader/Copied/inner"),
        step("move other host", Some("admin"), "MOVE", "/mokuro-reader/Copied").h("destination", "http://elsewhere:1/mokuro-reader/X"),
        step("propfind tree after moves", None, "PROPFIND", "/mokuro-reader").h("depth", "1"),
        step("lock file", Some("admin"), "LOCK", "/mokuro-reader/Renamed/moved.cbz").h("timeout", "Second-600").body(lockinfo).capture("lock-token", "token")
            .intended("Lock-Token is a Coded-URL <...> and Content-Type is application/xml (0.5.2: bare token, 'application; charset=utf-8')"),
        step("lock conflict", Some("editor"), "LOCK", "/mokuro-reader/Renamed/moved.cbz").body(lockinfo),
        step("put while locked", Some("admin"), "PUT", "/mokuro-reader/Renamed/moved.cbz").body(good.clone()),
        step("put with token", Some("admin"), "PUT", "/mokuro-reader/Renamed/moved.cbz").h("if", "(<{token}>)").body(good.clone())
            .intended("the ETag of an overwrite is the new file's (0.5.2 returned the old one, spec 14.4)"),
        step("unlock", Some("admin"), "UNLOCK", "/mokuro-reader/Renamed/moved.cbz").h("lock-token", "<{token}>"),
        step("lock missing", Some("admin"), "LOCK", "/mokuro-reader/Renamed/null.txt").body(lockinfo)
            .intended("LOCK of an unmapped URL is a lock-null 201 (0.5.2: 500 with a dangling lock, spec 14.7)"),
        step("proppatch", Some("admin"), "PROPPATCH", "/mokuro-reader/Renamed/moved.cbz").body(proppatch),
        step("propfind dead prop", Some("admin"), "PROPFIND", "/mokuro-reader/Renamed/moved.cbz").h("depth", "0").body(named_dead),
        step("delete file", Some("admin"), "DELETE", "/mokuro-reader/series/vol1.cbz"),
        step("propfind series after delete", None, "PROPFIND", "/mokuro-reader/series").h("depth", "1"),
        step("delete folder", Some("admin"), "DELETE", "/mokuro-reader/Copied"),
        step("delete 404", Some("admin"), "DELETE", "/mokuro-reader/Copied"),
        step("delete progress", Some("reader"), "DELETE", "/mokuro-reader/volume-data.json"),
        // --- the destructive 0.5.2 quirks (last: they make the trees diverge) --------
        step("put progress again", Some("reader"), "PUT", "/mokuro-reader/goals.json").body("{}"),
        step("move progress to library", Some("reader"), "MOVE", "/mokuro-reader/goals.json").h("destination", "{host}/mokuro-reader/series/x.cbz")
            .intended("cross-class MOVE is 403 (0.5.2: 201 and the progress file deleted, spec 14.1)"),
        step("copy file over collection", Some("admin"), "COPY", "/mokuro-reader/series/notes.txt").h("destination", "{host}/mokuro-reader/series")
            .intended("a file never replaces a collection: 409 (0.5.2 deleted the folder and answered 500, spec 14.2)"),
        step("delete reader root", Some("admin"), "DELETE", "/mokuro-reader")
            .intended("the reader root is never deleted: 403 (0.5.2: a no-op 204, spec 14.3)"),
        step("move reader root", Some("admin"), "MOVE", "/mokuro-reader").h("destination", "{host}/elsewhere")
            .intended("the reader root is never moved: 403 (0.5.2: 201 and the whole library deleted, spec 14.3)"),
        step("library after quirks", None, "PROPFIND", "/mokuro-reader").h("depth", "1")
            .intended("0.5.2 lost the library to the quirks above"),
    ]
}

/// One side's answer, normalised for comparison.
#[derive(Debug, PartialEq)]
struct Answer {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

/// Headers clients act on (value-normalised where the two sides legitimately differ).
const COMPARED: [&str; 14] = [
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

fn normalise(r: &Resp, exact: bool, method: &str) -> Answer {
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

fn canonical_xml(body: &[u8], exact: bool) -> String {
    match parse_xml(body) {
        Some(root) => canon(&root, exact, ""),
        None => format!("<unparsable: {}>", String::from_utf8_lossy(body)),
    }
}

fn substitute(v: &str, vars: &HashMap<String, String>) -> String {
    let mut out = v.to_string();
    for (k, val) in vars {
        out = out.replace(&format!("{{{k}}}"), val);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_matches_python_052() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let python = PathBuf::from(home).join(PYTHON);
    if !python.exists() {
        eprintln!(
            "SKIPPED: no Python 0.5.2 reference env at {}",
            python.display()
        );
        return;
    }
    let report = std::env::var_os("BUNKO_DAV_DIFF_REPORT").is_some();
    let work = tempfile::tempdir().unwrap();
    let py_base = work.path().join("py/storage");
    let rs_base = work.path().join("rs/storage");
    build_tree(&py_base);
    build_tree(&rs_base);

    let port = free_port();
    let worktree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let _server = Server(
        Command::new(&python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ref052_server.py"))
            .arg(&py_base)
            .arg(port.to_string())
            .current_dir(&worktree)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the Python reference server"),
    );
    let client = reqwest::Client::builder()
        .no_gzip()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let origin = format!("http://127.0.0.1:{port}");
    let mut up = false;
    for _ in 0..120 {
        if client
            .get(format!("{origin}/api/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(up, "the Python reference server did not come up");
    let dav = bunko_dav::Dav::new(
        &bunko_core::StorageLayout::new(&rs_base),
        bunko_dav::DavConfig::default(),
    )
    .unwrap();
    let hooks: std::sync::Arc<dyn DavHooks> = std::sync::Arc::new(NoHooks);
    let started = SystemTime::now();

    let mut py_vars: HashMap<String, String> =
        HashMap::from([("host".to_string(), origin.clone())]);
    let mut rs_vars = py_vars.clone();
    let mut unexpected = Vec::new();
    let mut intended_seen = Vec::new();
    for s in script() {
        // Python, over HTTP.
        let method = reqwest::Method::from_bytes(s.method.as_bytes()).unwrap();
        let mut rb = client.request(method, format!("{origin}{}", s.path));
        if let Some(u) = s.user {
            rb = rb.basic_auth(u, Some("pass1234"));
        }
        for (k, v) in &s.headers {
            rb = rb.header(*k, substitute(v, &py_vars));
        }
        let pr = rb.body(s.body.clone()).send().await.unwrap();
        let status = pr.status();
        let headers = pr.headers().clone();
        let body = pr.bytes().await.unwrap().to_vec();
        let py = Resp {
            status,
            headers,
            body,
        };

        // Rust, in process (the server has authorised the request already).
        let mut b = Request::builder()
            .method(s.method)
            .uri(s.path)
            .header("host", format!("127.0.0.1:{port}"));
        for (k, v) in &s.headers {
            b = b.header(*k, substitute(v, &rs_vars));
        }
        if !s.body.is_empty() {
            b = b.header("content-length", s.body.len().to_string());
        }
        let ctx = match s.user {
            None => DavContext::anonymous(hooks.clone()),
            Some(u) => DavContext::user(
                u,
                if u == "reader" {
                    Role::Registered
                } else if u == "uploader" {
                    Role::Uploader
                } else {
                    Role::Admin
                },
                hooks.clone(),
            ),
        };
        let rs = Resp::read(
            dav.handle(b.body(Body::from(s.body.clone())).unwrap(), ctx)
                .await,
        )
        .await;

        for (header, var) in s.capture.iter().copied() {
            for (resp, vars) in [(&py, &mut py_vars), (&rs, &mut rs_vars)] {
                if let Some(v) = resp.header(header) {
                    vars.insert(
                        var.to_string(),
                        v.trim_matches(|c| c == '<' || c == '>').to_string(),
                    );
                }
            }
        }
        let (a, b) = (
            normalise(&py, s.exact_times, s.method),
            normalise(&rs, s.exact_times, s.method),
        );
        let same = a == b;
        if report || !same {
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
            (false, None) => unexpected.push(s.name),
            (false, Some(why)) => intended_seen.push(format!("{}: {why}", s.name)),
            (true, Some(why)) => {
                eprintln!("   (intended difference did not show: {} -- {why})", s.name)
            }
            (true, None) => {}
        }
    }
    // The PUT-overwrite ETag fix, checked per side.
    eprintln!(
        "PUT overwrite ETag == later GET ETag: python {}, rust {}",
        py_vars.get("replace_etag") == py_vars.get("get_etag"),
        rs_vars.get("replace_etag") == rs_vars.get("get_etag")
    );
    assert_eq!(rs_vars.get("replace_etag"), rs_vars.get("get_etag"));
    eprintln!("intended differences ({}):", intended_seen.len());
    for d in &intended_seen {
        eprintln!("  - {d}");
    }
    eprintln!("script ran in {:?}", started.elapsed().unwrap_or_default());
    assert!(
        unexpected.is_empty(),
        "unexpected differences from 0.5.2: {unexpected:?}"
    );
}
