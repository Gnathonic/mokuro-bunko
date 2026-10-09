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
    /// libtorch packages (`tools/torch_export`'s `torch-models.json`): the compile
    /// target of an AOTInductor graph (`linux-cuda-sm_89`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The store directory a `.pt2` (a stored zip) is unpacked into at install; the
    /// runtime loads that directory in place and the zip is not kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpack_to: Option<String>,
    /// Manifest ids of the files a graph needs besides itself (its shared weights).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
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
                .map(|f| {
                    let mut m = local_mirror(f);
                    m.push(format!("{}/{f}", crate::models_release::RELEASE_BASE_URL));
                    m
                })
                .unwrap_or_default(),
            sha256: sha256.into(),
            size,
            license: "Apache-2.0".into(),
            source: Source {
                repo: PPOCR_REPO.into(),
                revision: PPOCR_REVISION.into(),
                path: file.into(),
            },
            target: None,
            unpack_to: None,
            requires: Vec::new(),
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
                mirrors: local_mirror(f.file),
                sha256: f.sha256.into(),
                size: f.size,
                license: "Apache-2.0".into(),
                source: Source {
                    repo: repo.into(),
                    revision: revision.into(),
                    path: format!("{}/{}", crate::models_release::RELEASE, f.file),
                },
                target: None,
                unpack_to: None,
                requires: Vec::new(),
            });
        }
        files.extend(torch_release_files());
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
        if let Some(dir) = self.unpacked_dir(file) {
            return Some(dir);
        }
        let path = self.store_path(file);
        fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file() && m.len() == file.size)
            .map(|_| path)
    }

    /// For a package entry (`unpack_to`): its unpacked directory, when the unpack of
    /// exactly the manifest's bytes completed (stamp `.unpacked` = its sha256).
    fn unpacked_dir(&self, file: &ModelFile) -> Option<PathBuf> {
        let dir = self.opts.root.join(file.unpack_to.as_deref()?);
        let stamp = fs::read_to_string(dir.join(UNPACKED_STAMP)).ok()?;
        let mut lines = stamp.lines();
        if lines.next().map(str::trim) != Some(file.sha256.as_str()) {
            return None;
        }
        if lines.next().map(str::trim) != Some(UNPACK_LAYOUT) {
            // Unpacked before the archive's top folder was stripped: move it up.
            flatten_unpacked(&dir, &file.sha256);
        }
        Some(dir)
    }

    /// Files and directories in the store that this build's manifest no longer names
    /// (a model a later release replaced), for [`ModelStore::prune`]. Kept: everything
    /// a manifest entry names (its file, its `.verified` stamp, its unpacked package
    /// directory and what is inside), hidden files and downloads in progress
    /// (`.part`), and every file outside the known layout's top level that is not a
    /// model (nothing else is ever written there by the store).
    pub fn unreferenced(&self) -> Vec<PathBuf> {
        let root = &self.opts.root;
        let mut keep_files: std::collections::HashSet<PathBuf> = Default::default();
        let mut keep_dirs: Vec<PathBuf> = Vec::new();
        for f in &self.manifest.files {
            let p = root.join(&f.path);
            let mut stamp = p.clone().into_os_string();
            stamp.push(".verified");
            keep_files.insert(PathBuf::from(stamp));
            keep_files.insert(p);
            if let Some(u) = &f.unpack_to {
                keep_dirs.push(root.join(u));
            }
        }
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.ends_with(".part") {
                    continue;
                }
                if keep_dirs.iter().any(|k| path.starts_with(k)) {
                    continue;
                }
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    // A directory some kept path lives under: look inside it.
                    if keep_files.iter().any(|k| k.starts_with(&path))
                        || keep_dirs.iter().any(|k| k.starts_with(&path))
                    {
                        stack.push(path);
                    } else if dir != *root || name == TORCH_DIR {
                        out.push(path);
                    }
                } else if !keep_files.contains(&path) {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// Delete [`ModelStore::unreferenced`] (after an update proved itself, so nothing
    /// running still needs the old files). Returns what was removed.
    pub fn prune(&self) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        for p in self.unreferenced() {
            let r = if p.is_dir() {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
            match r {
                Ok(()) => removed.push(p),
                Err(e) => warn!(path = %p.display(), "could not remove an old model file: {e}"),
            }
        }
        removed
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

    /// Local path of `id`, downloading it if needed and allowed. A package entry
    /// (`unpack_to`) resolves to its unpacked directory: the downloaded zip is verified,
    /// unpacked and deleted (one copy on disk).
    pub fn ensure(&self, id: &str) -> Result<Resolved> {
        let file = self.manifest.get(id).ok_or_else(|| Error::Model {
            id: id.into(),
            msg: "not in the manifest".into(),
        })?;
        if file.unpack_to.is_some() {
            return self.ensure_unpacked(file);
        }
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

    fn ensure_unpacked(&self, file: &ModelFile) -> Result<Resolved> {
        if let Some(dir) = self.unpacked_dir(file) {
            return Ok(Resolved {
                path: dir,
                verified: true,
            });
        }
        // An override directory's zip is used as it is (the runtime unpacks it).
        for candidate in self.override_candidates(file) {
            if candidate.is_file() {
                let verified = override_verified(&candidate, &file.sha256)?;
                return Ok(Resolved {
                    path: candidate,
                    verified,
                });
            }
        }
        let zip = self.store_path(file);
        let have = zip.is_file() && self.is_verified(file, &zip)?;
        if !have {
            if !self.opts.download {
                return Err(Error::Model {
                    id: file.id.clone(),
                    msg: format!(
                        "missing from {} and downloading is disabled; fetch {}",
                        zip.display(),
                        file.url
                    ),
                });
            }
            self.fetch(file, &zip)?;
        }
        let rel = file.unpack_to.as_deref().unwrap_or_default();
        let dir = self.opts.root.join(rel);
        unpack_zip(&zip, &dir, &file.sha256).map_err(|msg| Error::Model {
            id: file.id.clone(),
            msg,
        })?;
        let _ = fs::remove_file(&zip);
        let _ = fs::remove_file(stamp_of(&zip));
        info!(id = %file.id, dir = %dir.display(), "package unpacked");
        Ok(Resolved {
            path: dir,
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

/// The stamp an unpacked package directory carries: the sha256 of the zip it came from.
pub const UNPACKED_STAMP: &str = ".unpacked";
/// The stamp's second line: the layout of the unpack. `2`: the archive's one top folder
/// (`<role>/`, which `unpack_to` already names) stripped, so `data/aotinductor/` sits
/// right in `unpack_to`. A stamp without it is a 1 (`<role>/<role>/data/...`).
const UNPACK_LAYOUT: &str = "layout 2";

fn stamp_text(sha256: &str) -> String {
    format!("{sha256}\n{UNPACK_LAYOUT}\n")
}

/// The single top folder every entry of `names` sits in, when there is one and it is
/// not the package's own `data/` (strip it).
fn common_top<'a>(names: impl Iterator<Item = &'a Path>) -> Option<std::ffi::OsString> {
    let mut top: Option<std::ffi::OsString> = None;
    let mut nested = false;
    for n in names {
        let mut c = n.components();
        let first = c.next()?.as_os_str().to_owned();
        nested |= c.next().is_some();
        match &top {
            None => top = Some(first),
            Some(t) if *t == first => {}
            Some(_) => return None,
        }
    }
    top.filter(|t| nested && t != "data")
}

/// A layout-1 unpack (`<dir>/<role>/data/...`) moved to layout 2 in place; the stamp is
/// rewritten last. Best effort: a failure leaves the old layout, which still loads on
/// Linux and macOS.
fn flatten_unpacked(dir: &Path, sha256: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    let subdirs: Vec<&PathBuf> = entries
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n != UNPACKED_STAMP))
        .collect();
    let [inner] = subdirs.as_slice() else { return };
    if !inner.is_dir() || dir.join("data").exists() {
        return;
    }
    let moved = (|| -> std::io::Result<()> {
        for e in fs::read_dir(inner)?.flatten() {
            fs::rename(e.path(), dir.join(e.file_name()))?;
        }
        fs::remove_dir(inner)
    })();
    if moved.is_ok() {
        let _ = fs::write(dir.join(UNPACKED_STAMP), stamp_text(sha256));
    }
}

/// Unpacks `zip` into `dest` (replacing what is there): into a sibling temp directory
/// first (zip-slip checked), then renamed; the stamp is written last.
fn unpack_zip(zip: &Path, dest: &Path, sha256: &str) -> std::result::Result<(), String> {
    let parent = dest.parent().ok_or("no parent directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.part-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let result = (|| {
        let f = File::open(zip).map_err(|e| format!("{}: {e}", zip.display()))?;
        let mut archive = zip::ZipArchive::new(f).map_err(|e| format!("{}: {e}", zip.display()))?;
        // A `.pt2` holds one top folder named after its role, which `dest` already is.
        let names: Vec<PathBuf> = (0..archive.len())
            .map(|i| {
                let e = archive.by_index_raw(i).map_err(|e| e.to_string())?;
                e.enclosed_name()
                    .ok_or_else(|| format!("unsafe entry {:?} in {}", e.name(), zip.display()))
            })
            .collect::<std::result::Result<_, _>>()?;
        let top = common_top(names.iter().map(PathBuf::as_path));
        for (i, rel) in names.iter().enumerate() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
            let rel = match &top {
                Some(t) => rel.strip_prefix(t).unwrap_or(rel),
                None => rel.as_path(),
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let out = tmp.join(rel);
            if entry.is_dir() {
                fs::create_dir_all(&out).map_err(|e| e.to_string())?;
                continue;
            }
            if let Some(p) = out.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut w = File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            std::io::copy(&mut entry, &mut w).map_err(|e| format!("{}: {e}", out.display()))?;
        }
        fs::write(tmp.join(UNPACKED_STAMP), stamp_text(sha256)).map_err(|e| e.to_string())?;
        let _ = fs::remove_dir_all(dest);
        fs::rename(&tmp, dest).map_err(|e| format!("{}: {e}", dest.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
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

// ---------------------------------------------------------------------------
// libtorch (AOTInductor) packages, docs/rust-port/TORCH-BACKEND.md

/// Torch packages live under `<storage>/models/torch/<engine>/<precision>/<target>/`.
pub const TORCH_DIR: &str = "torch";
/// Dev/test override: a directory laid out like `<storage>/models/torch/`, searched
/// before the store (files there are used as they are).
pub const TORCH_MODELS_DIR_ENV: &str = "MOKURO_TORCH_MODELS_DIR";
/// The graphs of one package, each the `.pt2` zip AOTInductor writes or that zip
/// unpacked into a directory of the same name (loaded in place, nothing extracted).
pub const TORCH_GRAPHS: [&str; 3] = ["vision.pt2", "prefill.pt2", "step.pt2"];

/// `tools/torch_export`'s `torch-models.json` of the release this build uses (the
/// compiled libtorch packages and their shared weights), compiled in like
/// [`crate::models_release`]. Refresh it with each `torch-models-*` release.
const TORCH_MODELS_JSON: &str = include_str!("torch_models.json");

#[derive(Deserialize)]
struct TorchRelease {
    release: String,
    files: Vec<TorchReleaseFile>,
}

#[derive(Deserialize)]
struct TorchReleaseFile {
    engine: String,
    file: String,
    url: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    licence: String,
    #[serde(default)]
    sources: Vec<TorchReleaseSource>,
    #[serde(default)]
    role: String,
    #[serde(default)]
    precision: Option<String>,
    id: String,
    path: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    unpack_to: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
}

#[derive(Deserialize)]
struct TorchReleaseSource {
    repo: String,
    revision: String,
}

fn torch_release() -> Option<TorchRelease> {
    serde_json::from_str(TORCH_MODELS_JSON).ok()
}

/// The torch package release this build fetches from (`torch-models-v1`), for the
/// sidecars' `ocr_engine.weights` provenance.
pub fn torch_release_name() -> String {
    torch_release().map_or_else(|| "unknown".into(), |r| r.release)
}

/// A mirror of the `models-v1` release (and the PP-OCR files under their release
/// names), tried first: a base URL, or a directory holding the release assets under
/// their flat names (air-gapped hosts, an offline package).
pub const MODELS_MIRROR_ENV: &str = "MOKURO_MODELS_MIRROR";

/// `[<MOKURO_MODELS_MIRROR>/<file>]`, or nothing when the variable is unset.
fn local_mirror(file: &str) -> Vec<String> {
    std::env::var(MODELS_MIRROR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| {
            let v = v.trim().trim_end_matches('/').to_string();
            let base = if v.contains("://") {
                v
            } else {
                format!("file://{}", expand_home(Path::new(&v)).display())
            };
            vec![format!("{base}/{file}")]
        })
        .unwrap_or_default()
}

/// A mirror of the torch package release, tried before GitHub: a base URL, or a
/// directory holding the release assets under their flat names (air-gapped hosts,
/// a local copy of the release, tests).
pub const TORCH_MIRROR_ENV: &str = "MOKURO_TORCH_MODELS_MIRROR";

/// The compiled packages as manifest entries (id = store path). None when the
/// compiled-in release does not load (a broken build: a test keeps it loading), logged.
fn torch_release_files() -> Vec<ModelFile> {
    torch_release_files_from(TORCH_MODELS_JSON).unwrap_or_else(|e| {
        tracing::error!("the compiled-in torch package release does not load: {e}");
        Vec::new()
    })
}

/// [`torch_release_files`] of one `torch-models.json`. Each `requires` entry names
/// another file of the release by id (as `tools/torch_export` writes it) or by its
/// flat release file name; one that names neither fails the load, so a package
/// never silently loses the weights it binds.
fn torch_release_files_from(json: &str) -> std::result::Result<Vec<ModelFile>, String> {
    let rel: TorchRelease =
        serde_json::from_str(json).map_err(|e| format!("torch-models.json: {e}"))?;
    let mirror = std::env::var(TORCH_MIRROR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| {
            let v = v.trim().trim_end_matches('/').to_string();
            if v.contains("://") {
                v
            } else {
                format!("file://{}", expand_home(Path::new(&v)).display())
            }
        });
    let ids: std::collections::HashSet<&str> = rel.files.iter().map(|f| f.id.as_str()).collect();
    let by_file: std::collections::HashMap<&str, &str> = rel
        .files
        .iter()
        .map(|f| (f.file.as_str(), f.id.as_str()))
        .collect();
    rel.files
        .iter()
        .map(|f| {
            let requires = f
                .requires
                .iter()
                .map(|r| {
                    if ids.contains(r.as_str()) {
                        Ok(r.clone())
                    } else if let Some(id) = by_file.get(r.as_str()) {
                        Ok(id.to_string())
                    } else {
                        Err(format!(
                            "{} requires {r}, which is not a file of the release",
                            f.id
                        ))
                    }
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let src = f.sources.first();
            Ok(ModelFile {
                engine: f.engine.clone(),
                role: f.role.clone(),
                precision: f.precision.clone(),
                part_of: None,
                id: f.id.clone(),
                path: f.path.clone(),
                url: f.url.clone(),
                mirrors: mirror.iter().map(|m| format!("{m}/{}", f.file)).collect(),
                sha256: f.sha256.clone(),
                size: f.size,
                license: if f.licence.is_empty() {
                    "Apache-2.0".into()
                } else {
                    f.licence.clone()
                },
                source: Source {
                    repo: src.map(|s| s.repo.clone()).unwrap_or_default(),
                    revision: src.map(|s| s.revision.clone()).unwrap_or_default(),
                    path: format!("{}/{}", rel.release, f.file),
                },
                target: f.target.clone(),
                unpack_to: f.unpack_to.clone(),
                requires,
            })
        })
        .collect()
}

/// `torch/<engine>/<precision>/<target>`: a package directory relative to the store.
pub fn torch_package_rel(engine: &str, precision: &str, target: &str) -> String {
    format!("{TORCH_DIR}/{engine}/{precision}/{target}")
}

/// The manifest id (and store path) of one file of a package:
/// `torch/<engine>/<precision>/<target>/<file>`. The shared weights of GPU packages are
/// `torch/<engine>/<precision>/weights-<group>.safetensors`.
pub fn torch_manifest_id(engine: &str, precision: &str, target: &str, file: &str) -> String {
    format!("{}/{file}", torch_package_rel(engine, precision, target))
}

/// CUDA architectures `tools/torch_export` compiles for (SASS + PTX each), newest first.
pub const TORCH_CUDA_ARCHS: [u32; 6] = [120, 90, 89, 86, 80, 75];

/// The compiled-package targets that run on a device, best first. Names are
/// `tools/torch_export`'s `<os>-<backend>-<arch>` (`os` = `std::env::consts::OS`):
///
/// * CUDA (`kind` `cuda`, `arch` `sm_89`): `<os>-cuda-sm_89`, then every older
///   architecture's package (their PTX is JIT-compiled by the driver for newer cards).
/// * ROCm (`rocm`, `gfx1201`): `linux-rocm-gfx1201` only (gfx code is not portable).
/// * x86-64 CPU: fp32 `<os>-cpu-x86_64-v3` (AVX2+FMA hosts); bf16
///   `<os>-cpu-x86_64-v4bf16` (AVX-512 with AVX512_BF16).
/// * arm64 CPU: `<os>-cpu-arm64`.
pub fn torch_targets(kind: &str, arch: &str, isa: &[String], precision: &str) -> Vec<String> {
    let os = std::env::consts::OS;
    let has = |f: &str| isa.iter().any(|i| i == f);
    match kind {
        "cuda" => {
            let Some(sm) = arch.strip_prefix("sm_").and_then(|n| n.parse::<u32>().ok()) else {
                return Vec::new();
            };
            TORCH_CUDA_ARCHS
                .iter()
                .filter(|&&a| a <= sm)
                .map(|a| format!("{os}-cuda-sm_{a}"))
                .collect()
        }
        "rocm" if !arch.is_empty() => vec![format!("{os}-rocm-{arch}")],
        "cpu" => match arch {
            "x86_64" if precision == "bf16" => {
                let v4 = ["avx512f", "avx512bw", "avx512vl", "avx512dq"];
                if v4.iter().all(|f| has(f)) && has("avx512_bf16") {
                    vec![format!("{os}-cpu-x86_64-v4bf16")]
                } else {
                    Vec::new()
                }
            }
            "x86_64" if has("avx2") && has("fma") => vec![format!("{os}-cpu-x86_64-v3")],
            "aarch64" => vec![format!("{os}-cpu-arm64")],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// A package directory that has every graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorchPackage {
    pub dir: PathBuf,
    pub target: String,
}

fn torch_override_dir() -> Option<PathBuf> {
    std::env::var_os(TORCH_MODELS_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(|v| expand_home(Path::new(&v)))
}

/// A graph of a package directory: `<role>.pt2` (zip or unpacked directory), or the
/// directory the installer unpacked it to (`<role>/`).
pub fn torch_graph_path(dir: &Path, graph: &str) -> Option<PathBuf> {
    let unpacked = dir.join(graph.trim_end_matches(".pt2"));
    let has_data = |d: &Path| {
        d.join("data").is_dir()
            || fs::read_dir(d).is_ok_and(|rd| rd.flatten().any(|e| e.path().join("data").is_dir()))
    };
    if unpacked.join(UNPACKED_STAMP).is_file() || (unpacked.is_dir() && has_data(&unpacked)) {
        return Some(unpacked);
    }
    let p = dir.join(graph);
    p.exists().then_some(p)
}

fn has_graphs(dir: &Path) -> bool {
    TORCH_GRAPHS
        .iter()
        .all(|g| torch_graph_path(dir, g).is_some())
}

impl ModelStore {
    /// The package directory of `target` in the store (whether or not it exists).
    pub fn torch_package_dir(&self, engine: &str, precision: &str, target: &str) -> PathBuf {
        self.opts
            .root
            .join(torch_package_rel(engine, precision, target))
    }

    /// File names of the shared weights the release's package of `target` binds (its
    /// graphs' `requires`; none for CPU packages and packages not in the release).
    fn torch_bound_weights(&self, engine: &str, precision: &str, target: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .torch_manifest_graphs(engine, precision, target)
            .unwrap_or_default()
            .iter()
            .flat_map(|g| g.requires.iter())
            .filter_map(|r| self.manifest.get(r))
            .filter_map(|w| w.path.rsplit('/').next().map(str::to_string))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Whether `dir` holds a whole package: its three graphs, and the shared weights
    /// the release binds to it beside it (`<engine>/<precision>/weights-*`, where the
    /// loader looks).
    fn torch_package_complete(
        &self,
        dir: &Path,
        engine: &str,
        precision: &str,
        target: &str,
    ) -> bool {
        has_graphs(dir)
            && dir.parent().is_some_and(|p| {
                self.torch_bound_weights(engine, precision, target)
                    .iter()
                    .all(|w| p.join(w).is_file())
            })
    }

    /// The first of `targets` whose package is wholly on disk, the weights its graphs
    /// bind included (the override directory first, then the store). No hashing, no
    /// downloads.
    pub fn locate_torch_package(
        &self,
        engine: &str,
        precision: &str,
        targets: &[String],
    ) -> Option<TorchPackage> {
        let over = torch_override_dir();
        for t in targets {
            let rel = format!("{engine}/{precision}/{t}");
            if let Some(o) = &over
                && self.torch_package_complete(&o.join(&rel), engine, precision, t)
            {
                return Some(TorchPackage {
                    dir: o.join(&rel),
                    target: t.clone(),
                });
            }
            let dir = self.torch_package_dir(engine, precision, t);
            if self.torch_package_complete(&dir, engine, precision, t) {
                return Some(TorchPackage {
                    dir,
                    target: t.clone(),
                });
            }
        }
        None
    }

    /// The manifest entries of one package's graphs, when the manifest has all three.
    fn torch_manifest_graphs(
        &self,
        engine: &str,
        precision: &str,
        target: &str,
    ) -> Option<Vec<&ModelFile>> {
        TORCH_GRAPHS
            .iter()
            .map(|g| {
                self.manifest
                    .get(&torch_manifest_id(engine, precision, target, g))
            })
            .collect()
    }

    /// Whether a package for one of `targets` is on disk, or listed in the manifest
    /// and downloads are allowed.
    pub fn torch_package_obtainable(
        &self,
        engine: &str,
        precision: &str,
        targets: &[String],
    ) -> bool {
        self.locate_torch_package(engine, precision, targets)
            .is_some()
            || (self.can_download()
                && targets
                    .iter()
                    .any(|t| self.torch_manifest_graphs(engine, precision, t).is_some()))
    }

    /// Whether the package for one of `targets` is fully here, its shared weights
    /// included (no download needed): `doctor`.
    pub fn torch_package_present(
        &self,
        engine: &str,
        precision: &str,
        targets: &[String],
    ) -> Option<TorchPackage> {
        for t in targets {
            if let Some(graphs) = self.torch_manifest_graphs(engine, precision, t) {
                let all_here = graphs.iter().all(|g| {
                    self.locate(&g.id).is_some()
                        && g.requires.iter().all(|r| self.locate(r).is_some())
                });
                if all_here {
                    return Some(TorchPackage {
                        dir: self.torch_package_dir(engine, precision, t),
                        target: t.clone(),
                    });
                }
            }
        }
        self.locate_torch_package(engine, precision, targets)
    }

    /// The package for the first of `targets` in the override directory, the manifest
    /// (fetched, verified and unpacked as needed, with exactly the shared weights its
    /// graphs bind: none for CPU packages) or the store.
    pub fn ensure_torch_package(
        &self,
        engine: &str,
        precision: &str,
        targets: &[String],
    ) -> Result<TorchPackage> {
        if let Some(o) = torch_override_dir() {
            for t in targets {
                let dir = o.join(format!("{engine}/{precision}/{t}"));
                if self.torch_package_complete(&dir, engine, precision, t) {
                    return Ok(TorchPackage {
                        dir,
                        target: t.clone(),
                    });
                }
            }
        }
        for t in targets {
            let Some(graphs) = self.torch_manifest_graphs(engine, precision, t) else {
                continue;
            };
            let mut ids: Vec<&str> = Vec::new();
            for g in &graphs {
                ids.push(&g.id);
                ids.extend(g.requires.iter().map(String::as_str));
            }
            ids.dedup();
            for id in ids {
                self.ensure(id)?;
            }
            return Ok(TorchPackage {
                dir: self.torch_package_dir(engine, precision, t),
                target: t.clone(),
            });
        }
        if let Some(p) = self.locate_torch_package(engine, precision, targets) {
            return Ok(p);
        }
        Err(Error::Model {
            id: format!("{TORCH_DIR}/{engine}/{precision}"),
            msg: format!(
                "no compiled {engine} {precision} package for this device (looked for {}) in {} or the {} release",
                targets.join(", "),
                self.opts.root.join(TORCH_DIR).display(),
                torch_release_name()
            ),
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
                target: None,
                unpack_to: None,
                requires: Vec::new(),
            }],
        }
    }

    #[test]
    fn packages_unpack_once_and_keep_no_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("vision.pt2");
        {
            let f = File::create(&src).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let o = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.add_directory("vision/", o).unwrap();
            z.start_file("vision/data/aotinductor/model/m.so", o)
                .unwrap();
            z.write_all(b"so").unwrap();
            z.finish().unwrap();
        }
        let bytes = fs::read(&src).unwrap();
        let mut m = tiny_manifest(&src);
        m.files[0].id = "torch/e/fp32/linux-cpu-x86_64-v3/vision.pt2".into();
        m.files[0].path = m.files[0].id.clone();
        m.files[0].unpack_to = Some("torch/e/fp32/linux-cpu-x86_64-v3/vision/".into());
        m.files[0].mirrors.clear();
        assert_eq!(m.files[0].size, bytes.len() as u64);
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: true,
            },
            m,
        );
        let id = "torch/e/fp32/linux-cpu-x86_64-v3/vision.pt2";
        assert!(store.locate(id).is_none());
        let r = store.ensure(id).unwrap();
        let dir = tmp
            .path()
            .join("models/torch/e/fp32/linux-cpu-x86_64-v3/vision");
        assert_eq!(r.path, dir);
        // the archive's top `vision/` is stripped: `data/` sits right in `unpack_to`
        assert!(dir.join("data/aotinductor/model/m.so").is_file());
        assert!(!dir.join("vision").exists());
        assert!(
            !store.store_path(&store.manifest().files[0]).exists(),
            "zip removed"
        );
        assert_eq!(store.locate(id), Some(dir.clone()));
        let pkg = dir.parent().unwrap();
        assert_eq!(torch_graph_path(pkg, "vision.pt2"), Some(dir.clone()));
        // the source is gone: a second ensure must not need it
        fs::remove_file(&src).unwrap();
        assert!(store.ensure(id).is_ok());

        // An unpack of the old layout (`vision/vision/data`, stamp without a layout line)
        // still counts, and is moved to the new layout in place.
        let sha = store.manifest().files[0].sha256.clone();
        fs::remove_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join("vision/data/aotinductor/model")).unwrap();
        fs::write(dir.join("vision/data/aotinductor/model/m.so"), b"so").unwrap();
        fs::write(dir.join(UNPACKED_STAMP), &sha).unwrap();
        assert_eq!(store.locate(id), Some(dir.clone()));
        assert!(dir.join("data/aotinductor/model/m.so").is_file());
        assert!(!dir.join("vision").exists());
        assert_eq!(
            fs::read_to_string(dir.join(UNPACKED_STAMP)).unwrap(),
            stamp_text(&sha)
        );
    }

    #[test]
    fn prune_keeps_what_the_manifest_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = ModelStore::new(
            StoreOptions {
                root: root.clone(),
                override_dir: None,
                download: false,
            },
            Manifest::builtin(),
        );
        let touch = |rel: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
            p
        };
        let m = Manifest::builtin();
        let kept_file = m
            .files
            .iter()
            .find(|f| f.unpack_to.is_none() && !f.path.contains('/'))
            .unwrap();
        let pkg = m.files.iter().find(|f| f.unpack_to.is_some()).unwrap();
        let keep = [
            touch(&kept_file.path),
            touch(&format!("{}.verified", kept_file.path)),
            touch(&format!("{}/data/a.so", pkg.unpack_to.as_deref().unwrap())),
            touch("hayai-nova_tokenizer.json.part"),
            touch(".lock"),
        ];
        let gone = [
            touch("hayai-nova_tokenizer-r0.json"),
            touch("hayai-nova_tokenizer-r0.json.verified"),
            touch("torch/hayai-nova/fp32/linux-cpu-x86_64-v0/vision/x.so"),
        ];
        let removed = store.prune();
        for p in &keep {
            assert!(p.exists(), "kept {}", p.display());
        }
        for p in &gone[..2] {
            assert!(!p.exists(), "removed {}", p.display());
        }
        assert!(
            !root
                .join("torch/hayai-nova/fp32/linux-cpu-x86_64-v0")
                .exists()
        );
        assert_eq!(removed.len(), 3, "{removed:?}");
    }

    #[test]
    fn common_top_strips_only_a_single_role_folder() {
        let p = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        let top = |v: &[PathBuf]| common_top(v.iter().map(PathBuf::as_path));
        assert_eq!(
            top(&p(&["vision/data/a", "vision/data/b"])),
            Some("vision".into())
        );
        assert_eq!(top(&p(&["data/a", "data/b"])), None);
        assert_eq!(top(&p(&["vision/a", "step/b"])), None);
        assert_eq!(top(&p(&["a.so"])), None);
    }

    #[test]
    fn the_torch_release_is_compiled_in() {
        let m = Manifest::builtin();
        let graphs: Vec<&ModelFile> = m.files.iter().filter(|f| f.unpack_to.is_some()).collect();
        assert!(!graphs.is_empty());
        for g in &graphs {
            assert!(g.id.starts_with("torch/") && g.id == g.path, "{}", g.id);
            assert!(g.target.is_some(), "{}", g.id);
            for r in &g.requires {
                assert!(m.get(r).is_some(), "{} requires {r}", g.id);
            }
            // every package, CPU ones too, binds the shared weights files
            assert!(!g.requires.is_empty(), "{} binds no weights", g.id);
        }
        assert!(torch_release_name().starts_with("torch-models"));
    }

    #[test]
    fn every_package_resolves_its_weights() {
        let files = torch_release_files_from(TORCH_MODELS_JSON).expect("compiled-in release loads");
        let m = Manifest::builtin();
        // Every package, GPU and CPU (Linux, Windows, macOS), is weightless: it binds the
        // shared weights files of its engine x precision.
        let packages: Vec<&ModelFile> = files.iter().filter(|f| f.unpack_to.is_some()).collect();
        let has = |os_backend: &str| {
            packages.iter().any(|g| {
                g.target
                    .as_deref()
                    .is_some_and(|t| t.starts_with(os_backend))
            })
        };
        for t in [
            "linux-cuda-",
            "linux-rocm-",
            "windows-cuda-",
            "linux-cpu-",
            "windows-cpu-",
            "macos-cpu-",
        ] {
            assert!(has(t), "{t}* packages are in the release");
        }
        for g in packages {
            assert!(!g.requires.is_empty(), "{} binds no weights", g.id);
            for r in &g.requires {
                let w = m.get(r).unwrap_or_else(|| panic!("{} requires {r}", g.id));
                assert!(w.path.ends_with(".safetensors"), "{r}");
            }
        }
    }

    #[test]
    fn a_release_requires_entry_resolves_by_id_or_file_name_or_fails() {
        let rel = |requires: &str| {
            format!(
                r#"{{"release":"torch-models-test","files":[
                {{"engine":"hayai-nova","file":"w.safetensors","id":"torch/hayai-nova/bf16/weights-vision.safetensors",
                  "path":"torch/hayai-nova/bf16/weights-vision.safetensors","url":"u","sha256":"00","size":1}},
                {{"engine":"hayai-nova","file":"g.pt2","id":"torch/hayai-nova/bf16/linux-cuda-sm_80/vision.pt2",
                  "path":"torch/hayai-nova/bf16/linux-cuda-sm_80/vision.pt2","url":"u","sha256":"00","size":1,
                  "target":"linux-cuda-sm_80","unpack_to":"vision.pt2","requires":["{requires}"]}}]}}"#
            )
        };
        let want = "torch/hayai-nova/bf16/weights-vision.safetensors";
        for by in [want, "w.safetensors"] {
            let files = torch_release_files_from(&rel(by)).unwrap();
            assert_eq!(files[1].requires, vec![want.to_string()], "by {by}");
        }
        let err = torch_release_files_from(&rel("torch/hayai-nova/bf16/weights-nope.safetensors"))
            .unwrap_err();
        assert!(err.contains("weights-nope"), "{err}");
    }

    #[test]
    fn torch_targets_per_device() {
        let os = std::env::consts::OS;
        let isa = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let t = |v: &[&str]| v.iter().map(|s| format!("{os}-{s}")).collect::<Vec<_>>();
        assert_eq!(
            torch_targets("cuda", "sm_89", &[], "bf16"),
            t(&["cuda-sm_89", "cuda-sm_86", "cuda-sm_80", "cuda-sm_75"])
        );
        assert_eq!(
            torch_targets("cuda", "sm_75", &[], "fp16"),
            t(&["cuda-sm_75"])
        );
        assert!(torch_targets("cuda", "sm_61", &[], "fp32").is_empty());
        assert_eq!(
            torch_targets("rocm", "gfx1201", &[], "fp32"),
            t(&["rocm-gfx1201"])
        );
        assert!(torch_targets("rocm", "", &[], "fp32").is_empty());
        let v3 = isa(&["avx2", "fma"]);
        let zen4 = isa(&[
            "avx2",
            "fma",
            "avx512f",
            "avx512bw",
            "avx512vl",
            "avx512dq",
            "avx512_bf16",
        ]);
        assert_eq!(
            torch_targets("cpu", "x86_64", &v3, "fp32"),
            t(&["cpu-x86_64-v3"])
        );
        assert!(torch_targets("cpu", "x86_64", &v3, "bf16").is_empty());
        assert_eq!(
            torch_targets("cpu", "x86_64", &zen4, "bf16"),
            t(&["cpu-x86_64-v4bf16"])
        );
        assert_eq!(
            torch_targets("cpu", "x86_64", &zen4, "fp32"),
            t(&["cpu-x86_64-v3"])
        );
        assert!(torch_targets("cpu", "x86_64", &isa(&["sse4.2"]), "fp32").is_empty());
        assert_eq!(
            torch_targets("cpu", "aarch64", &[], "fp32"),
            t(&["cpu-arm64"])
        );
    }

    #[test]
    fn torch_packages_on_disk_and_in_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("g.pt2");
        fs::write(&src, b"graph").unwrap();
        let sha = hex::encode(Sha256::digest(b"graph"));
        let mut manifest = Manifest { files: Vec::new() };
        for g in TORCH_GRAPHS {
            let id = torch_manifest_id("hayai-nova", "bf16", "rocm-gfx1201", g);
            manifest.files.push(ModelFile {
                engine: "hayai-nova".into(),
                role: "graph".into(),
                precision: Some("bf16".into()),
                part_of: None,
                path: id.clone(),
                id,
                url: format!("file://{}", src.display()),
                mirrors: Vec::new(),
                sha256: sha.clone(),
                size: 5,
                license: "Apache-2.0".into(),
                source: Source {
                    repo: "x/y".into(),
                    revision: "r".into(),
                    path: g.into(),
                },
                target: Some("rocm-gfx1201".into()),
                unpack_to: None,
                requires: vec!["torch/hayai-nova/bf16/weights-decoder.safetensors".into()],
            });
        }
        let weights = "torch/hayai-nova/bf16/weights-decoder.safetensors".to_string();
        let mut w = manifest.files[0].clone();
        w.id = weights.clone();
        w.path = weights.clone();
        w.requires = Vec::new();
        manifest.files.push(w);
        // a weights file nothing requires is never fetched
        let mut unused = manifest.files[3].clone();
        unused.id = "torch/hayai-nova/bf16/weights-vision.safetensors".into();
        unused.path = unused.id.clone();
        manifest.files.push(unused);
        let opts = |download| StoreOptions {
            root: tmp.path().join("models"),
            override_dir: None,
            download,
        };
        let targets = vec!["rocm-gfx1100".to_string(), "rocm-gfx1201".to_string()];
        let offline = ModelStore::new(opts(false), manifest.clone());
        assert!(
            offline
                .locate_torch_package("hayai-nova", "bf16", &targets)
                .is_none()
        );
        assert!(!offline.torch_package_obtainable("hayai-nova", "bf16", &targets));
        assert!(
            offline
                .ensure_torch_package("hayai-nova", "bf16", &targets)
                .is_err()
        );
        let online = ModelStore::new(opts(true), manifest);
        assert!(online.torch_package_obtainable("hayai-nova", "bf16", &targets));
        assert!(!online.torch_package_obtainable("hayai-nova", "fp16", &targets));
        let p = online
            .ensure_torch_package("hayai-nova", "bf16", &targets)
            .unwrap();
        assert_eq!(p.target, "rocm-gfx1201");
        assert!(
            tmp.path().join("models").join(&weights).is_file(),
            "shared weights fetched"
        );
        assert!(
            !tmp.path()
                .join("models/torch/hayai-nova/bf16/weights-vision.safetensors")
                .exists(),
            "weights no graph requires are not fetched"
        );
        assert_eq!(
            p.dir,
            tmp.path().join("models/torch/hayai-nova/bf16/rocm-gfx1201")
        );
        // now on disk: found without the manifest, by an offline store too
        let found = offline
            .locate_torch_package("hayai-nova", "bf16", &targets)
            .unwrap();
        assert_eq!(found, p);
        // graphs without the weights they bind are not a package
        let w_path = tmp.path().join("models").join(&weights);
        let w_bytes = fs::read(&w_path).unwrap();
        fs::remove_file(&w_path).unwrap();
        assert!(
            offline
                .locate_torch_package("hayai-nova", "bf16", &targets)
                .is_none()
        );
        assert!(
            offline
                .torch_package_present("hayai-nova", "bf16", &targets)
                .is_none()
        );
        fs::write(&w_path, w_bytes).unwrap();
        // an unpacked graph directory counts as a graph
        let dir = tmp
            .path()
            .join("models/torch/paddle-manga/fp32/cpu-x86_64-v3");
        for g in TORCH_GRAPHS {
            fs::create_dir_all(dir.join(g)).unwrap();
        }
        let cpu = vec!["cpu-x86_64-v3".to_string()];
        assert_eq!(
            offline
                .locate_torch_package("paddle-manga", "fp32", &cpu)
                .unwrap()
                .dir,
            dir
        );
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
        let models_v1 = |e: &str| -> Vec<&ModelFile> {
            m.engine_files(e)
                .into_iter()
                .filter(|f| !f.id.starts_with("torch/"))
                .collect()
        };
        let hayai = models_v1("hayai-nova");
        assert_eq!(hayai.len(), 8);
        let paddle = models_v1("paddle-manga");
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
