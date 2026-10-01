//! The model store: a typed manifest of model files and their local copies under
//! `<storage>/models/`, verified by sha256, downloaded on first use (resumable, then
//! an atomic rename).
//!
//! Sources are tried in order: the manifest's mirror URLs (GitHub release assets,
//! to be filled when the `models-v1` release exists), then the upstream URL (the
//! Hugging Face `resolve/<pinned revision>/` URL — never `main`). `file://` URLs
//! copy a local file, which is how air-gapped hosts and tests seed the store.
//!
//! Dev override: `MOKURO_MODELS_DIR` (or 0.5.2's `MOKURO_PPOCR_MODELS`) names a
//! directory searched first, in the store's own layout, the upstream repo layout and
//! flat. Files found there are used as they are; they count as `pinned` only if
//! their sha256 matches the manifest.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::error::{Error, Result};

/// Environment variable naming a directory of model files to use instead of the store.
pub const MODELS_DIR_ENV: &str = "MOKURO_MODELS_DIR";
/// 0.5.2's name for the same override (PP-OCR files only), still honoured.
pub const LEGACY_MODELS_DIR_ENV: &str = "MOKURO_PPOCR_MODELS";
/// `0`, `false`, `no` or `off` disables downloads.
pub const DOWNLOAD_ENV: &str = "MOKURO_MODELS_DOWNLOAD";
const LEGACY_DOWNLOAD_ENV: &str = "MOKURO_PPOCR_DOWNLOAD";

/// Where a model file comes from upstream (recorded in sidecar provenance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// e.g. `Kellenok/PP-OCRv6_manga` on Hugging Face.
    pub repo: String,
    /// The pinned commit.
    pub revision: String,
    /// Path inside the repo.
    pub path: String,
}

/// One file of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    /// The engine the file belongs to (`ppocr-manga`, `hayai-nova`, `paddle-manga`).
    #[serde(default)]
    pub engine: String,
    /// What the file is to its engine (`detector`, `vision`, `decoder`, `embed`, ...).
    #[serde(default)]
    pub role: String,
    /// `fp32` / `fp16` for weights that come in both; none for shared files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<String>,
    /// For ONNX external data: the `.onnx` file (manifest id) that loads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_of: Option<String>,
    /// Stable id, e.g. `ppocr-manga/det-v0.2`.
    pub id: String,
    /// Path under `<storage>/models/`.
    pub path: String,
    /// Upstream download URL.
    pub url: String,
    /// Mirrors tried before `url` (GitHub release assets).
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Lowercase hex sha256.
    pub sha256: String,
    pub size: u64,
    /// SPDX licence id.
    pub license: String,
    pub source: Source,
}

/// The list of model files this build knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<ModelFile>,
}

/// Pinned PP-OCRv6 manga repository and revision ("v0.2", 2026-09-28).
pub const PPOCR_REPO: &str = "Kellenok/PP-OCRv6_manga";
pub const PPOCR_REVISION: &str = "ba1d479e8a61a20e8318c9758c73fbbbd290b98d";
/// Manifest ids of the PP-OCR manga files (fp32).
pub const PPOCR_DETECTOR: &str = "ppocr-manga/det-v0.2";
pub const PPOCR_RECOGNIZER: &str = "ppocr-manga/rec-v0.2";
pub const PPOCR_DICTIONARY: &str = "ppocr-manga/dict-v6";

/// The upstream repo an exported engine's weights come from (the sidecar's
/// `ocr_engine.weights` names every source; this is the primary one).
fn release_source(engine: &str) -> (&'static str, &'static str) {
    match engine {
        "hayai-nova" => (
            "JustANormalTinkerer/hayai-ocr-v2.5-nova",
            "e46d79138499600564f810d44ab6bdea7230dee1",
        ),
        "paddle-manga" => (
            "sorryhyun/paddleocr-vl-1.6-manga-lora",
            "26292839d1469c14212a12a1e01b5b1fe01bff15",
        ),
        _ => ("", ""),
    }
}

fn hf(repo: &str, rev: &str, path: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/{rev}/{path}")
}

impl Manifest {
    /// The manifest compiled into this build.
    pub fn builtin() -> Self {
        let release_name = |id: &str| {
            crate::models_release::PPOCR_RELEASE_NAMES
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, f)| *f)
        };
        let ppocr = |id: &str, file: &str, role: &str, sha256: &str, size: u64| ModelFile {
            engine: "ppocr-manga".into(),
            role: role.into(),
            precision: file.ends_with(".onnx").then(|| "fp32".into()),
            part_of: None,
            id: id.into(),
            path: format!("ppocr-manga/{}", file.rsplit('/').next().unwrap_or(file)),
            url: hf(PPOCR_REPO, PPOCR_REVISION, file),
            // The models-v1 release carries the same bytes under a flat name.
            mirrors: release_name(id)
                .map(|f| vec![format!("{}/{f}", crate::models_release::RELEASE_BASE_URL)])
                .unwrap_or_default(),
            sha256: sha256.into(),
            size,
            license: "Apache-2.0".into(),
            source: Source {
                repo: PPOCR_REPO.into(),
                revision: PPOCR_REVISION.into(),
                path: file.into(),
            },
        };
        let mut files = vec![
            ppocr(
                PPOCR_DETECTOR,
                "det/manga_det_v0.2.onnx",
                "detector",
                "d132078c46e292b226fb5a2ca52a7612ad319262dfdf493e7d8e3be435295978",
                1_816_954,
            ),
            ppocr(
                PPOCR_RECOGNIZER,
                "rec/manga_rec_v0.2.onnx",
                "recognizer",
                "de12c84c63e62c80339e882e675983d886670dcb6f0147e1ed041afd6fa81888",
                21_167_540,
            ),
            ppocr(
                PPOCR_DICTIONARY,
                "ppocrv6_dict.txt",
                "dictionary",
                "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
                74_947,
            ),
        ];
        for f in crate::models_release::FILES {
            let (repo, revision) = release_source(f.engine);
            files.push(ModelFile {
                engine: f.engine.into(),
                role: f.role.into(),
                precision: f.precision.map(str::to_string),
                part_of: f.part_of.and_then(|onnx| {
                    crate::models_release::FILES
                        .iter()
                        .find(|g| g.file == onnx)
                        .map(|g| g.id.to_string())
                }),
                id: f.id.into(),
                // Flat, as released: an `.onnx` finds its external data beside it by name.
                path: f.file.into(),
                url: format!("{}/{}", crate::models_release::RELEASE_BASE_URL, f.file),
                mirrors: Vec::new(),
                sha256: f.sha256.into(),
                size: f.size,
                license: "Apache-2.0".into(),
                source: Source {
                    repo: repo.into(),
                    revision: revision.into(),
                    path: format!("{}/{}", crate::models_release::RELEASE, f.file),
                },
            });
        }
        Manifest { files }
    }

    /// Every file of one engine (all precisions).
    pub fn engine_files(&self, engine: &str) -> Vec<&ModelFile> {
        self.files.iter().filter(|f| f.engine == engine).collect()
    }

    pub fn get(&self, id: &str) -> Option<&ModelFile> {
        self.files.iter().find(|f| f.id == id)
    }
}

/// How the store looks for and fetches files.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// `<storage>/models`.
    pub root: PathBuf,
    /// A directory searched first (dev override).
    pub override_dir: Option<PathBuf>,
    /// Allowed to download missing files.
    pub download: bool,
}

impl StoreOptions {
    /// Store under `<storage>/models`, with the override and download switch from the
    /// environment.
    pub fn from_env(storage: &Path) -> Self {
        let dir = std::env::var_os(MODELS_DIR_ENV)
            .or_else(|| std::env::var_os(LEGACY_MODELS_DIR_ENV))
            .filter(|v| !v.is_empty())
            .map(|v| expand_home(Path::new(&v)));
        let flag = std::env::var(DOWNLOAD_ENV)
            .or_else(|_| std::env::var(LEGACY_DOWNLOAD_ENV))
            .unwrap_or_default();
        let flag = flag.trim().to_ascii_lowercase();
        let download = !matches!(flag.as_str(), "0" | "false" | "no" | "off");
        Self {
            root: storage.join("models"),
            override_dir: dir,
            download,
        }
    }
}

fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| p.to_path_buf()),
        Err(_) => p.to_path_buf(),
    }
}

/// A file the store resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    /// The bytes match the manifest's sha256.
    pub verified: bool,
}

/// The model store.
#[derive(Debug, Clone)]
pub struct ModelStore {
    opts: StoreOptions,
    manifest: Manifest,
}

/// sha256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

impl ModelStore {
    pub fn new(opts: StoreOptions, manifest: Manifest) -> Self {
        Self { opts, manifest }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn options(&self) -> &StoreOptions {
        &self.opts
    }

    /// Path of a manifest file inside the store (whether or not it exists).
    pub fn store_path(&self, file: &ModelFile) -> PathBuf {
        self.opts.root.join(&file.path)
    }

    /// Where the override directory may hold `file`: the store's layout, the upstream
    /// repo layout, the bare upstream name, and the release asset name.
    fn override_candidates(&self, file: &ModelFile) -> Vec<PathBuf> {
        let Some(dir) = &self.opts.override_dir else {
            return Vec::new();
        };
        let mut out = vec![dir.join(&file.path), dir.join(&file.source.path)];
        if let Some(name) = Path::new(&file.source.path).file_name() {
            out.push(dir.join(name));
        }
        for url in file.mirrors.iter().chain(std::iter::once(&file.url)) {
            if let Some(name) = url.rsplit('/').next().filter(|n| !n.is_empty()) {
                out.push(dir.join(name));
            }
        }
        out.dedup();
        out
    }

    /// The local copy of `id` if there is one, without hashing or downloading:
    /// an override-directory file, or a store file of the manifest's size.
    pub fn locate(&self, id: &str) -> Option<PathBuf> {
        let file = self.manifest.get(id)?;
        if let Some(found) = self
            .override_candidates(file)
            .into_iter()
            .find(|c| c.is_file())
        {
            return Some(found);
        }
        let path = self.store_path(file);
        fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file() && m.len() == file.size)
            .map(|_| path)
    }

    /// Whether missing files may be fetched (the `download` feature and
    /// `MOKURO_MODELS_DOWNLOAD`).
    pub fn can_download(&self) -> bool {
        self.opts.download && cfg!(feature = "download")
    }

    /// Hash the local copy of `id` against the manifest (`models verify`): `None`
    /// when there is no local copy.
    pub fn verify(&self, id: &str) -> Result<Option<(PathBuf, bool)>> {
        let Some(path) = self.locate(id) else {
            return Ok(None);
        };
        let file = self.manifest.get(id).ok_or_else(|| Error::Model {
            id: id.into(),
            msg: "not in the manifest".into(),
        })?;
        let ok = sha256_file(&path)? == file.sha256;
        Ok(Some((path, ok)))
    }

    /// Local path of `id`, downloading it if needed and allowed.
    pub fn ensure(&self, id: &str) -> Result<Resolved> {
        let file = self.manifest.get(id).ok_or_else(|| Error::Model {
            id: id.into(),
            msg: "not in the manifest".into(),
        })?;
        for candidate in self.override_candidates(file) {
            if candidate.is_file() {
                let verified = override_verified(&candidate, &file.sha256)?;
                if !verified {
                    warn!(id, path = %candidate.display(), "model file from the override directory does not match the manifest");
                }
                return Ok(Resolved {
                    path: candidate,
                    verified,
                });
            }
        }
        let path = self.store_path(file);
        if path.is_file() {
            if self.is_verified(file, &path)? {
                return Ok(Resolved {
                    path,
                    verified: true,
                });
            }
            warn!(id, path = %path.display(), "stored model file fails verification; fetching it again");
        }
        if !self.opts.download {
            return Err(Error::Model {
                id: id.into(),
                msg: format!(
                    "missing from {} and downloading is disabled; fetch {}",
                    path.display(),
                    file.url
                ),
            });
        }
        self.fetch(file, &path)?;
        Ok(Resolved {
            path,
            verified: true,
        })
    }

    /// Size + sha256 check, remembered in a `<file>.verified` stamp (size, mtime) so
    /// multi-gigabyte files are hashed once.
    fn is_verified(&self, file: &ModelFile, path: &Path) -> Result<bool> {
        let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
        if meta.len() != file.size {
            return Ok(false);
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stamp_path = stamp_of(path);
        let stamp = format!("{} {} {}", file.sha256, meta.len(), mtime);
        if fs::read_to_string(&stamp_path).is_ok_and(|s| s.trim() == stamp) {
            return Ok(true);
        }
        let ok = sha256_file(path)? == file.sha256;
        if ok {
            let _ = fs::write(&stamp_path, &stamp);
        }
        Ok(ok)
    }

    fn fetch(&self, file: &ModelFile, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let part = path.with_extension(format!(
            "{}.part",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let mut errors: Vec<String> = Vec::new();
        for url in file.mirrors.iter().chain(std::iter::once(&file.url)) {
            info!(id = %file.id, %url, size = file.size, "downloading model file");
            match fetch_to(url, &part, file.size) {
                Ok(()) => {
                    let got = sha256_file(&part)?;
                    if got != file.sha256 {
                        let _ = fs::remove_file(&part);
                        errors.push(format!("{url}: sha256 {got} != {}", file.sha256));
                        continue;
                    }
                    fs::rename(&part, path).map_err(|e| Error::io(path, e))?;
                    let _ = self.is_verified(file, path);
                    info!(id = %file.id, path = %path.display(), "model file installed");
                    return Ok(());
                }
                Err(e) => errors.push(format!("{url}: {e}")),
            }
        }
        Err(Error::Model {
            id: file.id.clone(),
            msg: format!("download failed: {}", errors.join("; ")),
        })
    }
}

/// sha256 checks of override-directory files, remembered for the process by
/// (path, size, mtime): the override is never written to, and multi-gigabyte
/// exports should be hashed once, not at every session open.
fn override_verified(path: &Path, sha256: &str) -> Result<bool> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    type Key = (PathBuf, u64, u128, String);
    static SEEN: OnceLock<Mutex<HashMap<Key, bool>>> = OnceLock::new();
    let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = (path.to_path_buf(), meta.len(), mtime, sha256.to_string());
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&ok) = seen.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return Ok(ok);
    }
    let ok = sha256_file(path)? == sha256;
    seen.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, ok);
    Ok(ok)
}

fn stamp_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".verified");
    PathBuf::from(s)
}

/// Fetch `url` into `part`, resuming a previous partial download when the server
/// honours `Range`.
fn fetch_to(url: &str, part: &Path, expected: u64) -> Result<()> {
    if let Some(local) = url.strip_prefix("file://") {
        let src = Path::new(local);
        fs::copy(src, part).map_err(|e| Error::io(src, e))?;
        return Ok(());
    }
    download(url, part, expected)
}

#[cfg(feature = "download")]
fn download(url: &str, part: &Path, expected: u64) -> Result<()> {
    let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let have = if have >= expected { 0 } else { have };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    let mut req = agent.get(url);
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let err = |msg: String| Error::Download {
        url: url.to_string(),
        msg,
    };
    let resp = req.call().map_err(|e| err(e.to_string()))?;
    let resumed = resp.status().as_u16() == 206;
    let mut out = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(part)
        .map_err(|e| Error::io(part, e))?;
    let mut body = resp.into_body();
    let mut reader = body.as_reader();
    let mut buf = vec![0u8; 1 << 16];
    let started = std::time::Instant::now();
    let mut last_report = started;
    let mut got: u64 = if resumed { have } else { 0 };
    let name = url.rsplit('/').next().unwrap_or(url);
    loop {
        let n = reader.read(&mut buf).map_err(|e| err(e.to_string()))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| Error::io(part, e))?;
        got += n as u64;
        // Progress for the log: a multi-gigabyte model takes minutes.
        if last_report.elapsed() >= std::time::Duration::from_secs(5) {
            last_report = std::time::Instant::now();
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let fetched = got - if resumed { have } else { 0 };
            info!(
                "downloading {name}: {} / {} MB ({:.0}%), {:.1} MB/s",
                got / 1_000_000,
                expected / 1_000_000,
                100.0 * got as f64 / expected.max(1) as f64,
                fetched as f64 / 1e6 / secs
            );
        }
    }
    out.sync_all().map_err(|e| Error::io(part, e))?;
    Ok(())
}

#[cfg(not(feature = "download"))]
fn download(url: &str, _part: &Path, _expected: u64) -> Result<()> {
    Err(Error::Download {
        url: url.to_string(),
        msg: "this build has no downloader (feature `download`)".into(),
    })
}

/// The three PP-OCR manga files, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpocrFiles {
    pub detector: PathBuf,
    pub recognizer: PathBuf,
    pub dictionary: PathBuf,
    /// Every file matches the pinned manifest entry: the sidecar may claim the
    /// upstream revision.
    pub pinned: bool,
}

impl ModelStore {
    /// Resolve (and if needed fetch) the PP-OCR manga detector, recognizer and dictionary.
    pub fn ppocr(&self) -> Result<PpocrFiles> {
        let det = self.ensure(PPOCR_DETECTOR)?;
        let rec = self.ensure(PPOCR_RECOGNIZER)?;
        let dict = self.ensure(PPOCR_DICTIONARY)?;
        Ok(PpocrFiles {
            pinned: det.verified && rec.verified && dict.verified,
            detector: det.path,
            recognizer: rec.path,
            dictionary: dict.path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_manifest(src: &Path) -> Manifest {
        let bytes = fs::read(src).unwrap();
        let sha = hex::encode(Sha256::digest(&bytes));
        Manifest {
            files: vec![ModelFile {
                engine: "t".into(),
                role: "test".into(),
                precision: None,
                part_of: None,
                id: "t/one".into(),
                path: "t/one.bin".into(),
                url: format!("file://{}", src.display()),
                mirrors: vec!["file:///nonexistent/mirror.bin".into()],
                sha256: sha,
                size: bytes.len() as u64,
                license: "MIT".into(),
                source: Source {
                    repo: "x/y".into(),
                    revision: "r".into(),
                    path: "dir/one.bin".into(),
                },
            }],
        }
    }

    #[test]
    fn fetch_verify_and_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        fs::write(&src, b"hello model").unwrap();
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: true,
            },
            tiny_manifest(&src),
        );
        let r = store.ensure("t/one").unwrap();
        assert!(r.verified);
        assert_eq!(fs::read(&r.path).unwrap(), b"hello model");
        assert!(stamp_of(&r.path).is_file());
        // Corrupt it: verification fails and the file is fetched again.
        fs::write(&r.path, b"hello modeX").unwrap();
        assert!(store.ensure("t/one").unwrap().verified);
        assert_eq!(fs::read(&r.path).unwrap(), b"hello model");
    }

    #[test]
    fn override_dir_and_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        fs::write(&src, b"abc").unwrap();
        let dev = tmp.path().join("dev");
        fs::create_dir_all(dev.join("dir")).unwrap();
        fs::write(dev.join("dir/one.bin"), b"not it").unwrap();
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: Some(dev),
                download: false,
            },
            tiny_manifest(&src),
        );
        let r = store.ensure("t/one").unwrap();
        assert!(!r.verified);
        let offline = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models2"),
                override_dir: None,
                download: false,
            },
            tiny_manifest(&src),
        );
        assert!(offline.ensure("t/one").is_err());
    }

    /// Real HTTPS download (with a resumed partial file) of the PP-OCR dictionary.
    #[test]
    #[ignore = "network"]
    fn downloads_and_resumes_from_hugging_face() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: true,
            },
            Manifest::builtin(),
        );
        let file = store.manifest().get(PPOCR_DICTIONARY).unwrap().clone();
        let path = store.store_path(&file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A previous attempt left the first 1000 bytes behind.
        let full = {
            let other = ModelStore::new(
                StoreOptions {
                    root: tmp.path().join("ref"),
                    override_dir: None,
                    download: true,
                },
                Manifest::builtin(),
            );
            fs::read(other.ensure(PPOCR_DICTIONARY).unwrap().path).unwrap()
        };
        let part = path.with_extension("txt.part");
        fs::write(&part, &full[..1000]).unwrap();
        let r = store.ensure(PPOCR_DICTIONARY).unwrap();
        assert!(r.verified);
        assert_eq!(fs::read(&r.path).unwrap(), full);
        assert!(!part.exists());
    }

    #[test]
    fn builtin_manifest_is_pinned() {
        let m = Manifest::builtin();
        let ppocr = m.engine_files("ppocr-manga");
        assert_eq!(ppocr.len(), 3);
        for f in &ppocr {
            assert!(f.url.contains(PPOCR_REVISION));
            assert_eq!(f.sha256.len(), 64);
            assert!(f.mirrors[0].ends_with(&format!(
                "/models-v1/{}",
                crate::models_release::PPOCR_RELEASE_NAMES
                    .iter()
                    .find(|(i, _)| *i == f.id)
                    .unwrap()
                    .1
            )));
        }
        let hayai = m.engine_files("hayai-nova");
        assert_eq!(hayai.len(), 8);
        let paddle = m.engine_files("paddle-manga");
        assert_eq!(paddle.len(), 12);
        let data = m.get("paddle-manga/decoder-data-fp16").unwrap();
        assert_eq!(data.part_of.as_deref(), Some("paddle-manga/decoder-fp16"));
        assert_eq!(data.path, "paddle-manga_decoder_fp16.onnx.data");
        for f in hayai.iter().chain(&paddle) {
            assert!(f.url.starts_with(crate::models_release::RELEASE_BASE_URL));
            assert_eq!(f.sha256.len(), 64);
            assert!(!f.source.revision.is_empty());
        }
        let mut ids: Vec<&str> = m.files.iter().map(|f| f.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), m.files.len(), "ids are unique");
    }

    #[test]
    fn override_dir_finds_release_names() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ppocr-manga_dict.txt"), b"x").unwrap();
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: Some(tmp.path().to_path_buf()),
                download: false,
            },
            Manifest::builtin(),
        );
        let found = store.locate(PPOCR_DICTIONARY).unwrap();
        assert_eq!(found, tmp.path().join("ppocr-manga_dict.txt"));
        assert!(!store.ensure(PPOCR_DICTIONARY).unwrap().verified);
        assert!(store.locate(PPOCR_DETECTOR).is_none());
        assert_eq!(store.verify(PPOCR_DETECTOR).unwrap(), None);
        assert_eq!(
            store.verify(PPOCR_DICTIONARY).unwrap(),
            Some((found, false))
        );
    }
}
