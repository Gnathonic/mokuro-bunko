//! PUT bodies: staged beside the destination, size/digest/zip-checked, then atomically
//! renamed into place (spec §8.8), and the verdict a client gets for it (spec §9).

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;
use rand::RngCore;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::paths;

/// `Content-Digest` algorithms checked, in preference order (RFC 9530).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgo {
    Sha256,
    Sha512,
}

impl DigestAlgo {
    pub fn name(self) -> &'static str {
        match self {
            DigestAlgo::Sha256 => "sha-256",
            DigestAlgo::Sha512 => "sha-512",
        }
    }

    fn len(self) -> usize {
        match self {
            DigestAlgo::Sha256 => 32,
            DigestAlgo::Sha512 => 64,
        }
    }
}

/// `(algorithm, digest)` from a `Content-Digest` header (0.5.2 `parse_content_digest`).
/// Strict: any malformed member (bad base64, parameters, bare token, wrong length for a
/// known algorithm) ignores the whole header; unknown algorithms are skipped; a repeated
/// key keeps its last value; sha-256 is preferred over sha-512.
pub fn parse_content_digest(header: Option<&str>) -> Option<(DigestAlgo, Vec<u8>)> {
    let header = header?;
    if header.trim().is_empty() {
        return None;
    }
    let mut sha256: Option<Vec<u8>> = None;
    let mut sha512: Option<Vec<u8>> = None;
    for raw in header.split(',') {
        let member = raw.trim();
        let (key, value) = member.split_once('=')?;
        if !valid_key(key) {
            return None;
        }
        let b64 = value.strip_prefix(':')?.strip_suffix(':')?;
        if !valid_b64(b64) {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let algo = match key {
            "sha-256" => Some(DigestAlgo::Sha256),
            "sha-512" => Some(DigestAlgo::Sha512),
            _ => None,
        };
        if let Some(algo) = algo {
            if decoded.len() != algo.len() {
                return None;
            }
            match algo {
                DigestAlgo::Sha256 => sha256 = Some(decoded),
                DigestAlgo::Sha512 => sha512 = Some(decoded),
            }
        }
    }
    sha256
        .map(|d| (DigestAlgo::Sha256, d))
        .or_else(|| sha512.map(|d| (DigestAlgo::Sha512, d)))
}

/// `^[a-z*][a-z0-9_.*-]*$`
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '*' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.*-".contains(c))
}

/// `[A-Za-z0-9+/]*={0,2}`
fn valid_b64(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    s.len() - body.len() <= 2
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

enum Hasher {
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
}

impl Hasher {
    fn new(algo: DigestAlgo) -> Self {
        match algo {
            DigestAlgo::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            DigestAlgo::Sha512 => Hasher::Sha512(sha2::Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Hasher::Sha256(h) => h.finalize().to_vec(),
            Hasher::Sha512(h) => h.finalize().to_vec(),
        }
    }
}

/// Why a PUT body was not stored (0.5.2 `UploadOutcome` failure fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub status: u16,
    pub reason: &'static str,
    pub detail: String,
    pub retry: bool,
}

impl Rejection {
    fn new(status: u16, reason: &'static str, detail: impl Into<String>, retry: bool) -> Self {
        Self {
            status,
            reason,
            detail: detail.into(),
            retry,
        }
    }

    pub fn from_io(e: &io::Error, doing: &str) -> Self {
        if is_disk_full(e) {
            return Rejection::new(
                507,
                "disk-full",
                "The server's disk is full; the upload was not stored.",
                false,
            );
        }
        Rejection::new(
            500,
            "server-error",
            format!("The server failed {doing}: {}.", strerror(e)),
            true,
        )
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) || matches!(e.raw_os_error(), Some(28) | Some(122))
}

/// Python's `strerror` (the message without the `(os error N)` suffix Rust appends).
fn strerror(e: &io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// What a successful PUT stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// `verified` (a `.cbz` whose CRCs checked out) or `stored`.
    pub verdict: &'static str,
    pub size: u64,
    pub digest_verified: Option<&'static str>,
    /// The body equalled the destination's bytes, so nothing was replaced (only with
    /// [`Staging::keep_if_identical`]).
    pub unchanged: bool,
}

/// One body being staged at `<dest dir>/.<name>.upload-<random>.tmp`. Dropping it before
/// [`Staging::commit`] discards the staged bytes (an aborted or rejected upload never
/// touches the destination).
pub struct Staging {
    dest: PathBuf,
    temp: PathBuf,
    file: Option<tokio::fs::File>,
    hasher: Option<Hasher>,
    expected_digest: Option<(DigestAlgo, Vec<u8>)>,
    expected_size: Option<u64>,
    written: u64,
    finished: bool,
    keep_identical: bool,
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

impl Staging {
    pub async fn create(
        dest: &Path,
        expected_size: Option<u64>,
        expected_digest: Option<(DigestAlgo, Vec<u8>)>,
    ) -> Result<Staging, Rejection> {
        let dir = dest.parent().ok_or_else(|| {
            Rejection::new(
                500,
                "server-error",
                "The server failed writing the upload: no parent directory.",
                true,
            )
        })?;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| Rejection::from_io(&e, "writing the upload"))?;
        let name = paths::file_name(dest);
        for _ in 0..16 {
            let mut raw = [0u8; 6];
            rand::rng().fill_bytes(&mut raw);
            let suffix: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            let temp = dir.join(format!(".{name}.upload-{suffix}.tmp"));
            // Mode 0o666 & ~umask, the mode the published file must end with (0.5.2 staged
            // at 0600 and chmod'ed after the rename).
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await
            {
                Ok(file) => {
                    return Ok(Staging {
                        dest: dest.to_path_buf(),
                        temp,
                        file: Some(file),
                        hasher: expected_digest.as_ref().map(|(a, _)| Hasher::new(*a)),
                        expected_digest,
                        expected_size,
                        written: 0,
                        finished: false,
                        keep_identical: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Rejection::from_io(&e, "writing the upload")),
            }
        }
        Err(Rejection::new(
            500,
            "server-error",
            "The server failed writing the upload: no staging name.",
            true,
        ))
    }

    /// At commit, leave an existing destination alone when the body equals its bytes
    /// (a re-sent, unchanged file keeps its mtime and its history).
    pub fn keep_if_identical(&mut self) {
        self.keep_identical = true;
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), Rejection> {
        let file = self.file.as_mut().ok_or_else(|| {
            Rejection::new(
                500,
                "server-error",
                "The server failed writing the upload: closed.",
                true,
            )
        })?;
        file.write_all(data)
            .await
            .map_err(|e| Rejection::from_io(&e, "writing the upload"))?;
        if let Some(h) = self.hasher.as_mut() {
            h.update(data);
        }
        self.written += data.len() as u64;
        Ok(())
    }

    /// Flush, fsync and check the staged bytes, then rename them over the destination.
    /// `damage` remembers archive damage per destination (`.cbz` only).
    pub async fn commit(mut self, damage: &DamageMemory) -> Result<Stored, Rejection> {
        if let Some(mut file) = self.file.take() {
            file.flush()
                .await
                .map_err(|e| Rejection::from_io(&e, "saving the upload"))?;
            if let Err(e) = file.sync_all().await
                && !matches!(e.raw_os_error(), Some(22) | Some(95) | Some(30))
                && !matches!(
                    e.kind(),
                    io::ErrorKind::Unsupported
                        | io::ErrorKind::InvalidInput
                        | io::ErrorKind::ReadOnlyFilesystem
                )
            {
                return Err(Rejection::from_io(&e, "saving the upload"));
            }
        }
        let size = tokio::fs::metadata(&self.temp)
            .await
            .map_err(|e| Rejection::from_io(&e, "checking the upload"))?
            .len();
        if let Some(expected) = self.expected_size
            && size != expected
        {
            return Err(Rejection::new(
                422,
                "truncated",
                format!("Received {size} of {expected} bytes; the upload was cut short."),
                true,
            ));
        }
        let mut digest_verified = None;
        if let (Some((algo, want)), Some(hasher)) =
            (self.expected_digest.take(), self.hasher.take())
        {
            if hasher.finish() != want {
                return Err(Rejection::new(
                    422,
                    "corrupted-in-transit",
                    format!(
                        "The upload does not match its {} Content-Digest: it was damaged on the way here. Sending it again should work.",
                        algo.name()
                    ),
                    true,
                ));
            }
            digest_verified = Some(algo.name());
        }
        let is_cbz = paths::py_suffix(&paths::file_name(&self.dest)).eq_ignore_ascii_case(".cbz");
        if is_cbz {
            let temp = self.temp.clone();
            let result = tokio::task::spawn_blocking(move || verify_staged(&temp))
                .await
                .map_err(|_| {
                    Rejection::new(
                        500,
                        "server-error",
                        "The server failed checking the upload: verifier panicked.",
                        true,
                    )
                })?;
            judge_archive(result, size, digest_verified, &self.dest, damage)?;
        }
        if self.keep_identical {
            let (temp, dest) = (self.temp.clone(), self.dest.clone());
            let same = tokio::task::spawn_blocking(move || same_bytes(&temp, &dest))
                .await
                .unwrap_or(false);
            if same {
                // Dropping `self` removes the staged copy.
                return Ok(Stored {
                    verdict: if is_cbz { "verified" } else { "stored" },
                    size,
                    digest_verified,
                    unchanged: true,
                });
            }
        }
        tokio::fs::rename(&self.temp, &self.dest)
            .await
            .map_err(|e| Rejection::from_io(&e, "moving the upload into place"))?;
        self.finished = true;
        damage.forget(&self.dest.to_string_lossy());
        Ok(Stored {
            verdict: if is_cbz { "verified" } else { "stored" },
            size,
            digest_verified,
            unchanged: false,
        })
    }
}

/// Do two files hold the same bytes? (Any read error: no.) Blocking.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(fa), Ok(fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    match (fa.metadata(), fb.metadata()) {
        (Ok(ma), Ok(mb)) if ma.is_file() && mb.is_file() && ma.len() == mb.len() => {}
        _ => return false,
    }
    let (mut ra, mut rb) = (fa, fb);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (Ok(n), Ok(m)) = (read_full(&mut ra, &mut ba), read_full(&mut rb, &mut bb)) else {
            return false;
        };
        if n != m || ba[..n] != bb[..n] {
            return false;
        }
        if n == 0 {
            return true;
        }
    }
}

/// Fill `buf` as far as the reader allows; the count read (short only at the end).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// What the zip's own CRCs say about a staged archive.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Verified {
    pub head: Vec<u8>,
    pub refused: Option<String>,
    pub structural: Option<String>,
    pub damaged: Vec<String>,
    pub members: usize,
}

impl Verified {
    pub fn ok(&self) -> bool {
        self.refused.is_none() && self.structural.is_none() && self.damaged.is_empty()
    }

    pub fn describe(&self) -> String {
        if let Some(r) = &self.refused {
            return r.clone();
        }
        if let Some(s) = &self.structural {
            return format!("not a readable zip ({s})");
        }
        if !self.damaged.is_empty() {
            return describe_damaged(&self.damaged);
        }
        format!("{} members verified", self.members)
    }
}

fn describe_damaged(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().take(5).map(|n| py_repr(n)).collect();
    let shown = shown.join(", ");
    if names.len() == 1 {
        return format!("{shown} fails its CRC-32 check");
    }
    let more = if names.len() > 5 {
        format!(" and {} more", names.len() - 5)
    } else {
        String::new()
    };
    format!("{shown}{more} fail their CRC-32 checks")
}

/// Python `repr(str)`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

const RATIO: f64 = 20.0;
const FLOOR: u64 = 256 * 1024 * 1024;
const CEILING: u64 = 16 * 1024 * 1024 * 1024;

/// The inflation an archive of `archive_size` bytes may reach.
fn inflate_allowance(archive_size: u64) -> u64 {
    CEILING.min(FLOOR.max((archive_size as f64 * RATIO) as u64))
}

/// Check a staged archive (0.5.2 `verify_archive` with the upload `InflateLimit`):
/// the inflation bound is decided from the central directory before a byte is inflated;
/// then every reachable member is read to its end against its CRC-32. Blocking.
pub fn verify_staged(path: &Path) -> Verified {
    verify_with_allowance(path, inflate_allowance)
}

fn verify_with_allowance(path: &Path, allowance: fn(u64) -> u64) -> Verified {
    let mut result = Verified::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        result.structural = Some("OSError: cannot open the staged upload".to_string());
        return result;
    };
    let mut head = [0u8; 4];
    let n = read_up_to(&mut file, &mut head);
    result.head = head[..n].to_vec();
    let archive_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            result.structural = Some(format!("BadZipFile: {e}"));
            return result;
        }
    };
    // Distinct names, last entry wins (the zip crate's name map already resolves that).
    let mut members: Vec<(usize, String, bool, u16, u64)> = Vec::new();
    for i in 0..archive.len() {
        match archive.by_index_raw(i) {
            Ok(f) => {
                #[allow(deprecated)]
                let method = f.compression().to_u16();
                members.push((i, f.name().to_string(), f.is_dir(), method, f.size()));
            }
            Err(e) => {
                result.structural = Some(format!("BadZipFile: {e}"));
                return result;
            }
        }
    }
    let reachable: Vec<&(usize, String, bool, u16, u64)> =
        members.iter().filter(|m| !m.2).collect();
    let mut odd: Vec<String> = reachable
        .iter()
        .filter(|m| m.3 != 0 && m.3 != 8)
        .map(|m| match m.3 {
            12 => "bzip2".to_string(),
            14 => "LZMA".to_string(),
            other => format!("method {other}"),
        })
        .collect();
    odd.sort();
    odd.dedup();
    if !odd.is_empty() {
        result.refused = Some(format!(
            "its pages are compressed with {}, which a reader cannot open; re-pack it as an ordinary (deflate) zip",
            odd.join(", ")
        ));
        return result;
    }
    let declared: u64 = reachable.iter().map(|m| m.4).sum();
    let allowed = allowance(archive_size);
    if declared > allowed {
        result.refused = Some(format!(
            "its pages declare {:.1} GiB for a {:.1} MiB archive, more than any volume inflates to",
            declared as f64 / 1024f64.powi(3),
            archive_size as f64 / 1024f64.powi(2)
        ));
        return result;
    }
    // The declared sizes are the uploader's word: the bytes that actually come out are
    // counted too. A member inflating past its declared size is damaged (and its reading
    // stops there); the running total may never pass `allowed`.
    let mut buf = vec![0u8; 64 * 1024];
    let mut inflated: u64 = 0;
    for (index, name, _, _, declared_size) in reachable {
        result.members += 1;
        let mut member = match archive.by_index(*index) {
            Ok(m) => m,
            Err(zip::result::ZipError::UnsupportedArchive(_)) => continue, // encrypted: skipped
            Err(_) => {
                result.damaged.push(name.clone());
                continue;
            }
        };
        let mut produced: u64 = 0;
        loop {
            match member.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    produced += n as u64;
                    inflated += n as u64;
                    if inflated > allowed {
                        result.refused = Some(format!(
                            "its pages inflate past {:.1} GiB for a {:.1} MiB archive, more than any volume inflates to",
                            allowed as f64 / 1024f64.powi(3),
                            archive_size as f64 / 1024f64.powi(2)
                        ));
                        return result;
                    }
                    if produced > *declared_size {
                        result.damaged.push(name.clone());
                        break;
                    }
                }
                Err(_) => {
                    result.damaged.push(name.clone());
                    break;
                }
            }
        }
    }
    result
}

fn read_up_to(file: &mut std::fs::File, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    use std::io::Seek;
    let _ = file.seek(io::SeekFrom::Start(0));
    n
}

/// The verdict on a verified archive (0.5.2 `_ValidatedCbzWriter._verify`).
fn judge_archive(
    result: Verified,
    size: u64,
    digest_verified: Option<&str>,
    dest: &Path,
    damage: &DamageMemory,
) -> Result<(), Rejection> {
    if result.ok() {
        return Ok(());
    }
    if let Some(refused) = &result.refused {
        return Err(Rejection::new(
            422,
            "archive-refused",
            format!("The archive was not accepted: {refused}."),
            false,
        ));
    }
    if result.structural.is_some() && result.head != b"PK\x03\x04" && result.head != b"PK\x05\x06" {
        return Err(Rejection::new(
            422,
            "not-an-archive",
            "The upload is not a zip archive, so it cannot be a .cbz.",
            false,
        ));
    }
    let described = result.describe();
    if let Some(algo) = digest_verified {
        return Err(Rejection::new(
            422,
            "archive-damaged",
            format!(
                "The archive arrived intact ({algo} matched) but is damaged: {described}. Your copy of it is damaged; re-import this volume."
            ),
            false,
        ));
    }
    let mut damaged = result.damaged.clone();
    damaged.sort();
    let signature = format!(
        "{size}\0{}\0{}",
        result.structural.clone().unwrap_or_default(),
        damaged.join("\0")
    );
    if damage.seen_before(&dest.to_string_lossy(), &signature) {
        return Err(Rejection::new(
            422,
            "archive-damaged",
            format!(
                "The archive is damaged: {described}. The same damage arrived twice, so your copy of it is damaged; re-import this volume."
            ),
            false,
        ));
    }
    Err(Rejection::new(
        422,
        "archive-damaged",
        format!(
            "The archive is damaged: {described}. It may have been damaged on the way here; sending it again may work."
        ),
        true,
    ))
}

/// Damage seen in digest-less archive PUTs, per destination (0.5.2 `DamageMemory`):
/// the same damage twice at the same path says the client's copy is damaged. LRU-bounded,
/// forgetful, in memory only.
pub struct DamageMemory {
    capacity: usize,
    ttl: Duration,
    seen: Mutex<VecDeque<(String, String, Instant)>>,
}

impl Default for DamageMemory {
    fn default() -> Self {
        Self::new(256, Duration::from_secs(3600))
    }
}

impl DamageMemory {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            seen: Mutex::new(VecDeque::new()),
        }
    }

    pub fn seen_before(&self, path: &str, signature: &str) -> bool {
        self.seen_before_at(path, signature, Instant::now())
    }

    pub fn seen_before_at(&self, path: &str, signature: &str, now: Instant) -> bool {
        let mut seen = self.seen.lock();
        let previous = seen
            .iter()
            .position(|(p, _, _)| p == path)
            .and_then(|i| seen.remove(i));
        let repeat = previous.is_some_and(|(_, sig, at)| {
            sig == signature && now.saturating_duration_since(at) <= self.ttl
        });
        seen.push_back((path.to_string(), signature.to_string(), now));
        while seen.len() > self.capacity {
            seen.pop_front();
        }
        repeat
    }

    pub fn forget(&self, path: &str) {
        self.seen.lock().retain(|(p, _, _)| p != path);
    }
}

/// Python `json.dumps` of a string (ASCII-escaped).
pub fn py_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// The failure body `{"ok": false, "reason", "detail", "retry"}` (Python separators).
pub fn failure_json(reason: &str, detail: &str, retry: bool) -> String {
    format!(
        "{{\"ok\": false, \"reason\": {}, \"detail\": {}, \"retry\": {}}}",
        py_json_str(reason),
        py_json_str(detail),
        if retry { "true" } else { "false" }
    )
}

/// `(status, reason, detail, retry)` for a failed PUT with no writer outcome, by the
/// status it got (0.5.2 `UploadMiddleware._failure`).
pub fn failure_for_status(code: u16) -> (u16, &'static str, String, bool) {
    match code {
        401 => (401, "forbidden", "Sign in to upload.".to_string(), false),
        403 => (
            403,
            "forbidden",
            "This account may not upload this file.".to_string(),
            false,
        ),
        429 => (
            429,
            "forbidden",
            "Too many failed sign-ins; wait and try again.".to_string(),
            false,
        ),
        507 => (
            507,
            "disk-full",
            "The server's disk is full.".to_string(),
            false,
        ),
        other => (
            other,
            "server-error",
            format!("The server could not store the upload ({other})."),
            other >= 500 || other == 408 || other == 423,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deflated one-member zip of `real` zero bytes whose headers declare `declared`.
    fn understated_zip(real: usize, declared: u32) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("p1.jpg", opts).unwrap();
        w.write_all(&vec![0u8; real]).unwrap();
        let mut bytes = w.finish().unwrap().into_inner();
        // Local header: uncompressed size at 22; central directory entry: at 24.
        bytes[22..26].copy_from_slice(&declared.to_le_bytes());
        let cd = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bytes[cd + 24..cd + 28].copy_from_slice(&declared.to_le_bytes());
        bytes
    }

    /// Regression (review finding): verification trusted each member's declared size, so
    /// a member declaring 1 KiB could inflate without bound (CPU, and an "intact" verdict
    /// when its CRC matched). Real output is counted: past the declared size the member
    /// is damaged, and the total may never pass the allowance.
    #[test]
    fn understated_members_are_counted_by_what_they_inflate_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.cbz");
        std::fs::write(&path, understated_zip(4 << 20, 1024)).unwrap();
        let v = verify_with_allowance(&path, |_| 64 << 20);
        assert!(v.refused.is_none(), "{:?}", v.refused);
        assert_eq!(v.damaged, vec!["p1.jpg".to_string()]);
        assert!(!v.ok());
        // The declared 1 KiB fits a 2 KiB allowance; the real output does not.
        let v = verify_with_allowance(&path, |_| 2048);
        assert!(
            v.refused
                .as_deref()
                .is_some_and(|r| r.contains("inflate past")),
            "{:?}",
            v.refused
        );
        // Declared honestly but over the allowance once inflated: refused while reading.
        std::fs::write(&path, understated_zip(4 << 20, 4 << 20)).unwrap();
        assert!(verify_with_allowance(&path, |_| 64 << 20).ok());
        let v = verify_with_allowance(&path, |_| 1 << 20);
        assert!(v.refused.is_some(), "declared check must refuse first");
        // An honest small archive still verifies under the real allowance.
        std::fs::write(&path, understated_zip(1000, 1000)).unwrap();
        assert!(verify_staged(&path).ok());
    }

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn content_digest_parsing() {
        let body = b"hello";
        let d256 = sha2::Sha256::digest(body).to_vec();
        let ok = format!("sha-256=:{}:", b64(&d256));
        assert_eq!(
            parse_content_digest(Some(&ok)),
            Some((DigestAlgo::Sha256, d256.clone()))
        );
        let mixed = format!("md5=:AAAAAAAAAAAAAAAAAAAAAA==:, {ok}");
        assert_eq!(
            parse_content_digest(Some(&mixed)).unwrap().0,
            DigestAlgo::Sha256
        );
        assert_eq!(
            parse_content_digest(Some("md5=:AAAAAAAAAAAAAAAAAAAAAA==:")),
            None
        );
        for bad in [
            "sha-256=abc",
            "sha-256=:not base64!:",
            "sha-256=:AAAA:",
            "sha-256",
            "",
            "Sha-256=:AAAA:",
        ] {
            assert_eq!(parse_content_digest(Some(bad)), None, "{bad}");
        }
        assert_eq!(parse_content_digest(Some(&format!("{ok};param=1"))), None);
        let d512 = sha2::Sha512::digest(body).to_vec();
        let both = format!("sha-512=:{}:, {ok}", b64(&d512));
        assert_eq!(
            parse_content_digest(Some(&both)).unwrap().0,
            DigestAlgo::Sha256
        );
    }

    #[test]
    fn damage_memory_bounded_and_expiring() {
        let m = DamageMemory::new(2, Duration::from_secs(3600));
        let t0 = Instant::now();
        assert!(!m.seen_before_at("/a", "x", t0));
        assert!(m.seen_before_at("/a", "x", t0));
        m.seen_before_at("/b", "y", t0);
        m.seen_before_at("/c", "z", t0);
        assert!(!m.seen_before_at("/a", "x", t0));
        assert!(!m.seen_before_at("/a", "x", t0 + Duration::from_secs(3601)));
    }

    #[test]
    fn json_and_repr() {
        assert_eq!(
            failure_json("forbidden", "Sign in to upload.", false),
            r#"{"ok": false, "reason": "forbidden", "detail": "Sign in to upload.", "retry": false}"#
        );
        assert_eq!(py_json_str("\u{3a9} \"x\""), "\"\\u03a9 \\\"x\\\"\"");
        assert_eq!(py_repr("a.jpg"), "'a.jpg'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(
            describe_damaged(&["a".into(), "b".into()]),
            "'a', 'b' fail their CRC-32 checks"
        );
    }
}
