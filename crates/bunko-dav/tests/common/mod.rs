//! Shared helpers for the bunko-dav integration tests: a temp storage tree, a recording
//! `DavHooks`, request builders and archive makers.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use bunko_core::{Role, StorageLayout};
use bunko_dav::{AuditEvent, Dav, DavConfig, DavContext, DavHooks, PutFollowUp};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::BodyExt;
use parking_lot::Mutex;

/// Every hook call, as text, in order.
#[derive(Default)]
pub struct Recorder {
    pub calls: Mutex<Vec<String>>,
    pub audits: Mutex<Vec<AuditEvent>>,
    pub follow_up: Mutex<Option<PutFollowUp>>,
}

impl Recorder {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
    pub fn has(&self, needle: &str) -> bool {
        self.calls.lock().iter().any(|c| c == needle)
    }
    pub fn audits(&self) -> Vec<AuditEvent> {
        self.audits.lock().clone()
    }
    fn push(&self, s: String) {
        self.calls.lock().push(s);
    }
}

impl DavHooks for Recorder {
    fn audit(&self, event: AuditEvent) {
        self.push(format!(
            "audit {} {} {}",
            event.action, event.target_type, event.target_path
        ));
        self.audits.lock().push(event);
    }
    fn record_volume_upload(&self, rel: &str, actor: &str, existed_before: bool) {
        self.push(format!(
            "record_volume_upload {rel} {actor} {existed_before}"
        ));
    }
    fn forget_volume_upload(&self, rel: &str) {
        self.push(format!("forget_volume_upload {rel}"));
    }
    fn forget_volume_uuid(&self, rel: &str) {
        self.push(format!("forget_volume_uuid {rel}"));
    }
    fn forget_ocr_sidecars_of_volume(&self, rel: &str) {
        self.push(format!("forget_ocr_sidecars_of_volume {rel}"));
    }
    fn forget_ocr_sidecar(&self, rel: &str) {
        self.push(format!("forget_ocr_sidecar {rel}"));
    }
    fn rename_volume_upload(&self, old: &str, new: &str) {
        self.push(format!("rename_volume_upload {old} {new}"));
    }
    fn forget_volume_uploads_under_prefix(&self, p: &str) {
        self.push(format!("forget_volume_uploads_under_prefix {p}"));
    }
    fn forget_ocr_sidecars_under_prefix(&self, p: &str) {
        self.push(format!("forget_ocr_sidecars_under_prefix {p}"));
    }
    fn forget_volume_uuids_under_prefix(&self, p: &str) {
        self.push(format!("forget_volume_uuids_under_prefix {p}"));
    }
    fn rename_ocr_sidecars_under_prefix(&self, o: &str, n: &str) {
        self.push(format!("rename_ocr_sidecars_under_prefix {o} {n}"));
    }
    fn rename_volume_uuids_under_prefix(&self, o: &str, n: &str) {
        self.push(format!("rename_volume_uuids_under_prefix {o} {n}"));
    }
    fn primary_sidecar_leaving(&self, rel: &str, _sidecar: &Path) {
        self.push(format!("primary_sidecar_leaving {rel}"));
    }
    fn archives_removed(&self, paths: &[PathBuf]) {
        for p in paths {
            self.push(format!(
                "archives_removed {}",
                p.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    fn archive_arrived(&self, cbz: &Path) {
        self.push(format!(
            "archive_arrived {}",
            cbz.file_name().unwrap().to_string_lossy()
        ));
    }
    fn put_follow_up(&self, _cbz: &Path, series: &str, volume: &str) -> Option<PutFollowUp> {
        self.push(format!("put_follow_up {series} {volume}"));
        self.follow_up.lock().clone()
    }
    fn library_changed(&self, _paths: &[PathBuf]) {
        self.push("library_changed".to_string());
    }
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub base: PathBuf,
    pub dav: Dav,
    pub hooks: Arc<Recorder>,
}

impl Env {
    /// The fixture of `tests/integration/test_webdav_ops.py`.
    pub fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        std::fs::create_dir_all(base.join("library/thumbnails")).unwrap();
        std::fs::create_dir_all(base.join("inbox")).unwrap();
        std::fs::create_dir_all(base.join("users/reader")).unwrap();
        std::fs::create_dir_all(base.join("users/uploader")).unwrap();
        std::fs::write(base.join("library/manga1.cbz"), b"fake cbz content 1").unwrap();
        std::fs::write(base.join("library/manga2.cbz"), b"fake cbz content 2").unwrap();
        std::fs::create_dir_all(base.join("library/series")).unwrap();
        std::fs::write(base.join("library/series/vol1.cbz"), b"volume 1").unwrap();
        std::fs::write(
            base.join("users/reader/volume-data.json"),
            b"reader progress",
        )
        .unwrap();
        Self::with_base(dir, base)
    }

    pub fn with_base(dir: tempfile::TempDir, base: PathBuf) -> Env {
        let dav = Dav::new(&StorageLayout::new(&base), DavConfig::default()).unwrap();
        Env {
            dir,
            base,
            dav,
            hooks: Arc::new(Recorder::default()),
        }
    }

    pub fn lib(&self, rel: &str) -> PathBuf {
        self.base.join("library").join(rel)
    }

    pub fn ctx(&self, user: Option<&str>) -> DavContext {
        let hooks: Arc<dyn DavHooks> = self.hooks.clone();
        match user {
            None => DavContext::anonymous(hooks),
            Some(u) => {
                let role = match u {
                    "reader" => Role::Registered,
                    "uploader" => Role::Uploader,
                    "editor" => Role::Editor,
                    _ => Role::Admin,
                };
                DavContext::user(u, role, hooks)
            }
        }
    }

    pub async fn send(&self, user: Option<&str>, req: Request<Body>) -> Resp {
        let resp = self.dav.handle(req, self.ctx(user)).await;
        Resp::read(resp).await
    }

    pub async fn send_ctx(&self, ctx: DavContext, req: Request<Body>) -> Resp {
        Resp::read(self.dav.handle(req, ctx).await).await
    }

    /// Request with optional headers and body (Content-Length set from the body unless
    /// a header overrides it; `content_length: Some(None)` removes it, like chunked).
    pub async fn req(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Resp {
        self.send(user, request(method, path, headers, body)).await
    }
}

pub fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:8080");
    let mut has_len = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_len = true;
            if v.is_empty() {
                continue;
            }
        }
        b = b.header(*k, *v);
    }
    if !has_len && !body.is_empty() {
        b = b.header("content-length", body.len().to_string());
    }
    b.body(Body::from(body.to_vec())).unwrap()
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub async fn read(resp: Response<Body>) -> Resp {
        let (parts, body) = resp.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        Resp {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }
    pub fn code(&self) -> u16 {
        self.status.as_u16()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.text()))
    }
}

/// A zip like Python's `zipfile.ZipFile(..., "w")` writes (deflate), one page per index.
pub fn cbz_bytes(pages: usize) -> Vec<u8> {
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

/// Two stored members `000.jpg` (A*4000) and `001.jpg` (B*4000).
pub fn cbz_ab() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("000.jpg", opts).unwrap();
        zw.write_all(&[b'A'; 4000]).unwrap();
        zw.start_file("001.jpg", opts).unwrap();
        zw.write_all(&[b'B'; 4000]).unwrap();
        zw.finish().unwrap();
    }
    buf.into_inner()
}

fn flip_in(mut data: Vec<u8>, needle: u8) -> Vec<u8> {
    let at = data
        .windows(100)
        .position(|w| w.iter().all(|b| *b == needle))
        .unwrap();
    data[at + 50] ^= 0xFF;
    data
}

/// Directory intact, `001.jpg` fails its CRC.
pub fn damaged_cbz() -> Vec<u8> {
    flip_in(cbz_ab(), b'B')
}

/// Directory intact, `000.jpg` fails its CRC.
pub fn damaged_cbz_other() -> Vec<u8> {
    flip_in(cbz_ab(), b'A')
}

/// Every central-directory entry claims `uncompressed` bytes.
pub fn declaring(mut data: Vec<u8>, uncompressed: u32) -> Vec<u8> {
    let mut i = 0;
    while i + 4 <= data.len() {
        if &data[i..i + 4] == b"PK\x01\x02" {
            data[i + 24..i + 28].copy_from_slice(&uncompressed.to_le_bytes());
        }
        i += 1;
    }
    data
}

/// A stored zip whose member says it is bzip2 (method 12) everywhere.
pub fn bzip2_cbz() -> Vec<u8> {
    let mut data = cbz_ab();
    let mut i = 0;
    while i + 4 <= data.len() {
        if &data[i..i + 4] == b"PK\x03\x04" {
            data[i + 8..i + 10].copy_from_slice(&12u16.to_le_bytes());
        }
        if &data[i..i + 4] == b"PK\x01\x02" {
            data[i + 10..i + 12].copy_from_slice(&12u16.to_le_bytes());
        }
        i += 1;
    }
    data
}

pub fn digest_header(body: &[u8], algo: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let raw = if algo == "sha-512" {
        sha2::Sha512::digest(body).to_vec()
    } else {
        sha2::Sha256::digest(body).to_vec()
    };
    format!(
        "{algo}=:{}:",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// Dotfiles left in a folder (staging leftovers).
pub fn leftovers(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    v.sort();
    v
}

pub fn decode_gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap();
    out
}
