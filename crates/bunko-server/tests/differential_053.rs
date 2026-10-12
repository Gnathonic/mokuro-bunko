//! Differential test for 0.5.3's NTFS-style library paths: the same request script against
//! the Python 0.5.3 server (over HTTP) and against the whole Rust server (`assemble`, in
//! process: the path-case rewrite, auth, uploads, WebDAV, the database), each over its own
//! copy of one storage tree with case-variant folders. Every difference must be one of the
//! intended ones listed in the script.
//!
//! Needs `~/.cache/mokuro-bunko-demo/ref053/bin/python` (0.5.3 from commit 4016476,
//! installed editable from a scratch worktree; see CONVENTIONS.md); skipped with a message
//! when it is absent, or set `BUNKO_REQUIRE_REF_PYTHON=1` to fail instead.
//! `BUNKO_DAV_DIFF_REPORT=1` prints every step.

#[path = "../../bunko-dav/tests/common/diff.rs"]
mod diff;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use diff::{Step, Tally, set_mtime, step};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

const PYTHON: &str = ".cache/mokuro-bunko-demo/ref053/bin/python";

fn cbz(pages: usize) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for i in 0..pages {
            zw.start_file(format!("{i:03}.jpg"), opts).unwrap();
            zw.write_all(format!("fake image bytes {i}").as_bytes())
                .unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

/// The storage fixture: the shape of a library after 0.5.3's case merge (`Kingdom/` holding the
/// volumes, `第13巻.mokuro` among them), a folder whose name only differs in case from
/// a request's, a non-ASCII and an NFC-composed folder, a case-sensitive library that
/// already holds two spellings (`Both/`, `both/`), and a symlink leading out.
fn build_tree(base: &Path) {
    let lib = base.join("library");
    let dirs = [
        "thumbnails",
        "series",
        "Kingdom",
        "\u{c9}lan",
        "Pok\u{e9}mon",
        "Both",
        "both",
        "Links",
    ];
    for d in dirs {
        std::fs::create_dir_all(lib.join(d)).unwrap();
    }
    std::fs::create_dir_all(base.join("inbox")).unwrap();
    std::fs::create_dir_all(base.join("users/reader")).unwrap();
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("library/manga1.cbz", b"fake cbz content 1".to_vec()),
        ("library/manga2.cbz", b"fake cbz content 2".to_vec()),
        ("library/series/vol1.cbz", cbz(2)),
        ("library/series/vol1.mokuro", b"{}".to_vec()),
        ("library/Kingdom/第01巻.cbz", cbz(1)),
        (
            "library/Kingdom/第13巻.mokuro",
            b"{\"title\": \"Kingdom\"}".to_vec(),
        ),
        ("library/Both/a.cbz", cbz(1)),
        ("library/both/b.cbz", cbz(1)),
        ("users/reader/volume-data.json", b"reader progress".to_vec()),
    ];
    for (rel, data) in &files {
        std::fs::write(base.join(rel), data).unwrap();
        set_mtime(&base.join(rel));
    }
    let outside = base.parent().unwrap().join("outside.cbz");
    std::fs::write(&outside, b"untouched").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, lib.join("Links/out.cbz")).unwrap();
    for d in dirs {
        set_mtime(&lib.join(d));
    }
}

fn script() -> Vec<Step> {
    let lockinfo = r#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>me</D:owner></D:lockinfo>"#;
    let v13_mokuro = "/mokuro-reader/kingdom/%E7%AC%AC13%E5%B7%BB.mokuro";
    vec![
        // --- reads reach the on-disk spelling -------------------------------------------
        step("get a sidecar by the old spelling", None, "GET", v13_mokuro).exact(),
        step("get an archive in upper case", None, "GET", "/mokuro-reader/KINGDOM/%E7%AC%AC01%E5%B7%BB.CBZ").exact(),
        step("head a case variant", None, "HEAD", "/mokuro-reader/Series/VOL1.cbz").exact(),
        step("signed-in get by the old spelling", Some("reader"), "GET", v13_mokuro).exact(),
        step("propfind a case-variant folder d1", None, "PROPFIND", "/mokuro-reader/SERIES/").h("depth", "1").exact(),
        step("propfind a case-variant folder d0", None, "PROPFIND", "/mokuro-reader/kingdom").h("depth", "0").exact(),
        step("propfind a decomposed spelling", None, "PROPFIND", "/mokuro-reader/POKE%CC%81MON").h("depth", "0").exact(),
        step("propfind the exact lower spelling of two", None, "PROPFIND", "/mokuro-reader/both").h("depth", "1").exact(),
        step("propfind the exact upper spelling of two", None, "PROPFIND", "/mokuro-reader/Both").h("depth", "1").exact(),
        step("propfind a third spelling of two", None, "PROPFIND", "/mokuro-reader/BOTH").h("depth", "1").exact(),
        step("a per-user name in another case is a library path", Some("reader"), "PROPFIND", "/mokuro-reader/Volume-Data.json").h("depth", "0"),
        step("the per-user file itself", Some("reader"), "GET", "/mokuro-reader/volume-data.json").exact(),
        step("missing file in a case-variant folder", None, "GET", "/mokuro-reader/SERIES/nope.cbz"),
        // --- creating: a variant of what exists is what exists ---------------------------
        step("mkcol a case variant", Some("uploader"), "MKCOL", "/mokuro-reader/SERIES"),
        step("mkcol a new folder", Some("uploader"), "MKCOL", "/mokuro-reader/NewOne"),
        step("mkcol it in another case", Some("uploader"), "MKCOL", "/mokuro-reader/newone"),
        step("put into the folder by another case", Some("uploader"), "PUT", "/mokuro-reader/newone/v1.cbz").body(cbz(3)),
        step("another uploader's volume by another case", Some("uploader2"), "PUT", "/mokuro-reader/NEWONE/V1.cbz").body(cbz(3)),
        step("the owner's volume by another case", Some("uploader"), "PUT", "/mokuro-reader/NEWONE/V1.cbz").body(cbz(4))
            .intended("the ETag of an overwrite is the new file's (0.5.2/0.5.3 returned the old one, spec 14.4)"),
        step("propfind the new folder", None, "PROPFIND", "/mokuro-reader/NewOne").h("depth", "1"),
        step("put into a non-ASCII case variant", Some("uploader"), "PUT", "/mokuro-reader/%C3%A9lan/%E7%AC%AC13%E5%B7%BB.cbz").body(cbz(1)),
        step("put a volume into kingdom/", Some("uploader"), "PUT", "/mokuro-reader/kingdom/%E7%AC%AC02%E5%B7%BB.cbz").body(cbz(2)),
        step("put a sidecar into KINGDOM/", Some("admin"), "PUT", "/mokuro-reader/KINGDOM/%E7%AC%AC02%E5%B7%BB.MOKURO").body("{\"pages\": []}"),
        step("the library has no new spelling", None, "PROPFIND", "/mokuro-reader/").h("depth", "1"),
        step("Kingdom holds them", None, "PROPFIND", "/mokuro-reader/KINGDOM").h("depth", "1"),
        // --- case-only renames (0.5.2 answered 423) and moves onto variants ---------------
        step("rename a folder to fix its case", Some("admin"), "MOVE", "/mokuro-reader/series")
            .h("destination", "{host}/mokuro-reader/Series").h("overwrite", "T").h("depth", "infinity"),
        step("rename a file through a variant parent", Some("admin"), "MOVE", "/mokuro-reader/SERIES/vol1.cbz")
            .h("destination", "{host}/mokuro-reader/series/VOL1.cbz").h("overwrite", "F"),
        step("the renamed folder", None, "PROPFIND", "/mokuro-reader/Series").h("depth", "1"),
        step("move onto another file's variant", Some("admin"), "MOVE", "/mokuro-reader/manga1.cbz")
            .h("destination", "{host}/mokuro-reader/MANGA2.cbz").h("overwrite", "F"),
        step("copy onto its own variant", Some("admin"), "COPY", "/mokuro-reader/manga1.cbz")
            .h("destination", "{host}/mokuro-reader/MANGA1.cbz").h("overwrite", "T"),
        step("copy into a variant folder", Some("admin"), "COPY", "/mokuro-reader/manga1.cbz")
            .h("destination", "{host}/mokuro-reader/kingdom/copy.cbz"),
        step("rename a volume in place, encoded destination", Some("admin"), "MOVE", "/mokuro-reader/Kingdom/copy.cbz")
            .h("destination", "{host}/mokuro-reader/KINGDOM/Copy%20%E5%B7%BB.cbz"),
        step("Kingdom after the moves", None, "PROPFIND", "/mokuro-reader/kingdom").h("depth", "1"),
        // --- locks and deletes see one spelling ------------------------------------------
        step("lock by one spelling", Some("admin"), "LOCK", "/mokuro-reader/kingdom/%E7%AC%AC01%E5%B7%BB.cbz")
            .h("timeout", "Second-600").body(lockinfo).capture("lock-token", "token")
            .intended("Lock-Token is a Coded-URL <...> and Content-Type is application/xml (0.5.2: bare token, 'application; charset=utf-8')"),
        step("write by another while locked", Some("admin"), "PUT", "/mokuro-reader/KINGDOM/%E7%AC%AC01%E5%B7%BB.cbz").body(cbz(1)),
        step("unlock by another spelling", Some("admin"), "UNLOCK", "/mokuro-reader/KINGDOM/%E7%AC%AC01%E5%B7%BB.CBZ").h("lock-token", "<{token}>"),
        step("delete the owner's volume by another case", Some("uploader"), "DELETE", "/mokuro-reader/NEWONE/V1.CBZ"),
        step("delete another's volume by another case", Some("uploader2"), "DELETE", "/mokuro-reader/KINGDOM/%E7%AC%AC02%E5%B7%BB.CBZ")
            .intended("the auth gate's 403 text carries a Content-Length (0.5.3 sent it without one); same status and type"),
        // --- a destination symlink leading out of the library -------------------------------
        step("copy onto a symlink leading out", Some("admin"), "COPY", "/mokuro-reader/manga2.cbz")
            .h("destination", "{host}/mokuro-reader/Links/out.cbz").h("overwrite", "T")
            .intended("refused with 403 (0.5.3 wrote nothing either, but answered as if the copy was done)"),
        step("the library at the end", None, "PROPFIND", "/mokuro-reader/").h("depth", "1"),
    ]
}

async fn rust_send(
    app: &Router,
    port: u16,
    s: &Step,
    vars: &HashMap<String, String>,
) -> diff::Resp {
    let mut b = Request::builder()
        .method(s.method)
        .uri(s.path)
        .header("host", format!("127.0.0.1:{port}"));
    if let Some(u) = s.user {
        use base64::Engine;
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("{u}:pass1234"));
        b = b.header("authorization", format!("Basic {creds}"));
    }
    for (k, v) in &s.headers {
        b = b.header(*k, diff::substitute(v, vars));
    }
    if !s.body.is_empty() {
        b = b.header("content-length", s.body.len().to_string());
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(s.body.clone())).unwrap())
        .await
        .unwrap();
    let (parts, body) = resp.into_parts();
    diff::Resp {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_server_matches_python_053_path_case() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let python = PathBuf::from(home).join(PYTHON);
    if !python.exists() {
        assert!(
            std::env::var_os("BUNKO_REQUIRE_REF_PYTHON").is_none(),
            "no Python 0.5.3 reference env at {}",
            python.display()
        );
        eprintln!(
            "SKIPPED: no Python 0.5.3 reference env at {}",
            python.display()
        );
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let py_base = work.path().join("py/storage");
    let rs_base = work.path().join("rs/storage");
    build_tree(&py_base);
    build_tree(&rs_base);

    let port = diff::free_port();
    let client = diff::http_client();
    let _server = diff::start_reference(&python, &py_base, port, &client)
        .await
        .expect("the Python reference server did not come up");
    let origin = format!("http://127.0.0.1:{port}");

    let mut config = Config::default();
    config.storage.base_path = rs_base.clone();
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    for (name, role) in [
        ("reader", Role::Registered),
        ("uploader", Role::Uploader),
        ("editor", Role::Editor),
        ("admin", Role::Admin),
        ("uploader2", Role::Uploader),
    ] {
        services
            .db
            .create_user(name, "pass1234", role, UserStatus::Active, "")
            .unwrap();
    }
    let app = assemble(&services, &opts);

    let mut py_vars: HashMap<String, String> =
        HashMap::from([("host".to_string(), origin.clone())]);
    let mut rs_vars = py_vars.clone();
    let mut tally = Tally::new();
    for s in script() {
        let py = diff::python_send(&client, &origin, &s, &py_vars).await;
        let rs = rust_send(&app, port, &s, &rs_vars).await;
        tally.step(&s, &py, &rs, &mut py_vars, &mut rs_vars);
    }
    tally.finish("0.5.3");

    // Both trees ended up the same shape: no second spelling of any folder, and nothing
    // written through the symlink.
    let tree = |base: &Path| {
        let mut out: Vec<String> = Vec::new();
        let mut stack = vec![base.join("library")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                if rel.ends_with("series.json") || rel.ends_with("catalog.json") {
                    continue;
                }
                if e.file_type().unwrap().is_dir() {
                    stack.push(p);
                }
                out.push(rel);
            }
        }
        out.sort();
        out
    };
    assert_eq!(tree(&py_base), tree(&rs_base));
    for base in [&py_base, &rs_base] {
        assert_eq!(
            std::fs::read(base.parent().unwrap().join("outside.cbz")).unwrap(),
            b"untouched"
        );
    }
    eprintln!("final library: {:?}", tree(&rs_base));
    // Uploads and renames left the rows under the on-disk spelling.
    assert_eq!(
        services
            .db
            .get_volume_owner("Kingdom/第02巻.cbz")
            .unwrap()
            .as_deref(),
        Some("uploader")
    );
    assert_eq!(
        services.db.get_volume_owner("kingdom/第02巻.cbz").unwrap(),
        None
    );
}
