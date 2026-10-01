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

fn hf(repo: &str, rev: &str, path: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/{rev}/{path}")
}

impl Manifest {
    /// The manifest compiled into this build.
    pub fn builtin() -> Self {
        let ppocr = |id: &str, file: &str, sha256: &str, size: u64| ModelFile {
            id: id.into(),
            path: format!("ppocr-manga/{}", file.rsplit('/').next().unwrap_or(file)),
            url: hf(PPOCR_REPO, PPOCR_REVISION, file),
            mirrors: Vec::new(),
            sha256: sha256.into(),
            size,
            license: "Apache-2.0".into(),
            source: Source {
                repo: PPOCR_REPO.into(),
                revision: PPOCR_REVISION.into(),
                path: file.into(),
            },
        };
        Manifest {
            files: vec![
                ppocr(
                    PPOCR_DETECTOR,
                    "det/manga_det_v0.2.onnx",
                    "d132078c46e292b226fb5a2ca52a7612ad319262dfdf493e7d8e3be435295978",
                    1_816_954,
                ),
                ppocr(
                    PPOCR_RECOGNIZER,
                    "rec/manga_rec_v0.2.onnx",
                    "de12c84c63e62c80339e882e675983d886670dcb6f0147e1ed041afd6fa81888",
                    21_167_540,
                ),
                ppocr(
                    PPOCR_DICTIONARY,
                    "ppocrv6_dict.txt",
                    "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
                    74_947,
                ),
            ],
        }
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

    /// Local path of `id`, downloading it if needed and allowed.
    pub fn ensure(&self, id: &str) -> Result<Resolved> {
        let file = self.manifest.get(id).ok_or_else(|| Error::Model {
            id: id.into(),
            msg: "not in the manifest".into(),
        })?;
        if let Some(dir) = &self.opts.override_dir {
            let name = Path::new(&file.source.path)
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_default();
            for candidate in [
                dir.join(&file.path),
                dir.join(&file.source.path),
                dir.join(name),
            ] {
                if candidate.is_file() {
                    let verified = sha256_file(&candidate)? == file.sha256;
                    if !verified {
                        warn!(id, path = %candidate.display(), "model file from the override directory does not match the manifest");
                    }
                    return Ok(Resolved {
                        path: candidate,
                        verified,
                    });
                }
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
    loop {
        let n = reader.read(&mut buf).map_err(|e| err(e.to_string()))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| Error::io(part, e))?;
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
        assert_eq!(m.files.len(), 3);
        for f in &m.files {
            assert!(f.url.contains(PPOCR_REVISION));
            assert_eq!(f.sha256.len(), 64);
        }
    }
}
