//! OCR backend packs (docs/rust-port/TORCH-BACKEND.md, PACKAGING.md §8).
//!
//! The `mokuro-bunko` binary never links libtorch. The GPU/CPU recognizers come as a
//! **backend pack**: a directory `<root>/torch-<variant>-<torch version>/` holding the
//! libtorch runtime libraries (`lib/`), our `libbunko_torch` cdylib and `pack.json`
//! ([`PackManifest`]) with the sha256 of every file.
//!
//! A pack is published as one `.tar.zst` (split in parts when it is larger than a
//! GitHub release asset may be) listed in the signed `release.json` under `backends`
//! ([`BackendArtifact`]). Libraries we may not (or prefer not to) redistribute — the
//! NVIDIA CUDA libraries — are not in the archive: `pack.json` lists them as
//! [`ExternalArchive`]s (NVIDIA's own wheels on PyPI, pinned by sha256) and the
//! installer fetches them from there, the way 0.5.2's pip install did.
//!
//! Trust chain: release.json signature → archive sha256 → pack.json (inside the
//! archive) → every file's sha256, including the files taken from external wheels.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

/// `pack.json` format version this build reads and writes.
pub const PACK_FORMAT: u32 = 1;
/// The manifest's file name at the top of a pack.
pub const PACK_JSON: &str = "pack.json";

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("download failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{what}: sha256 is {got}, expected {want}")]
    Checksum {
        what: String,
        got: String,
        want: String,
    },
    #[error("{what}: size is {got} bytes, expected {want}")]
    Size { what: String, got: u64, want: u64 },
    #[error("invalid pack: {0}")]
    Invalid(String),
}

pub type Result<T, E = PackError> = std::result::Result<T, E>;

/// One file of a pack, relative to the pack directory (`/`-separated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// A file the installer takes out of an [`ExternalArchive`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalFile {
    /// Member path inside the archive (a wheel is a zip).
    pub from: String,
    /// Destination, relative to the pack directory.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// An upstream archive (a PyPI wheel) the installer downloads and unpacks part of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalArchive {
    /// e.g. `nvidia-cublas`.
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// Licence of the files (shown before downloading), e.g. `LicenseRef-NVIDIA-CUDA-EULA`.
    pub license: String,
    pub files: Vec<ExternalFile>,
}

/// What the host must provide for the pack to load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requires {
    /// Minimum NVIDIA driver (`580.65.06` for CUDA 13.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvidia_driver: Option<String>,
    /// Shared libraries the pack expects from the system (beyond libc/libstdc++).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_libs: Vec<String>,
    /// GPU architectures the runtime's kernel libraries were trimmed to (ROCm).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_archs: Vec<String>,
}

/// `pack.json`. A superset of `bunko_torch::abi::PackManifest` (what the loader in
/// bunko-engines reads: `format`, `variant`, `torch`, `abi`, `os`, `arch`, `library`,
/// `lib_dir`, `files`); the other fields are for the installer and humans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub format: u32,
    /// Directory name: `torch-<variant>-<torch>` (e.g. `torch-cu130-2.13.0`).
    pub name: String,
    /// `cpu`, `cu130`, `rocm7.1`.
    pub variant: String,
    /// libtorch version, e.g. `2.13.0`.
    pub torch: String,
    /// libtorch's `build-hash` (PyTorch git commit).
    #[serde(default)]
    pub torch_build: String,
    /// Rust target triple the pack is for.
    pub target: String,
    /// `std::env::consts::OS` / `ARCH` of the target (`linux`, `x86_64`).
    pub os: String,
    pub arch: String,
    /// `bt_abi_version()` of the cdylib (0: a pack built without one, for testing).
    pub abi: u32,
    /// The cdylib (`libbunko_torch.so`, `bunko_torch.dll`, `libbunko_torch.dylib`),
    /// relative to the pack directory; empty in a pack built without one.
    pub library: String,
    /// The libtorch runtime directory, relative to the pack.
    #[serde(default = "default_lib_dir")]
    pub lib_dir: String,
    /// mokuro-bunko version the cdylib was built from.
    #[serde(default)]
    pub bunko_version: String,
    #[serde(default)]
    pub requires: Requires,
    pub files: Vec<PackFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<ExternalArchive>,
    /// Aliases the installer creates (symlinks; copies where there are none): a library
    /// that others need under its SONAME when the distribution names the file
    /// differently (ROCm's `libamd_comgr.so` = `libamd_comgr.so.3`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<PackLink>,
}

/// `path` → `target`, both in the same directory of the pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackLink {
    pub path: String,
    /// A file name next to `path` (no directory part).
    pub target: String,
}

fn default_lib_dir() -> String {
    "lib".into()
}

impl PackManifest {
    pub fn dir_name(variant: &str, torch: &str) -> String {
        format!("torch-{variant}-{torch}")
    }

    /// Bytes the installer downloads on top of the archive.
    pub fn external_size(&self) -> u64 {
        self.external.iter().map(|e| e.size).sum()
    }

    /// Bytes on disk once installed.
    pub fn installed_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum::<u64>()
            + self
                .external
                .iter()
                .flat_map(|e| &e.files)
                .map(|f| f.size)
                .sum::<u64>()
    }

    pub fn parse(bytes: &[u8]) -> Result<PackManifest> {
        let m: PackManifest = serde_json::from_slice(bytes)
            .map_err(|e| PackError::Invalid(format!("pack.json: {e}")))?;
        if m.format != PACK_FORMAT {
            return Err(PackError::Invalid(format!(
                "pack.json format {} (this build reads {PACK_FORMAT})",
                m.format
            )));
        }
        for p in m
            .files
            .iter()
            .map(|f| &f.path)
            .chain(
                m.external
                    .iter()
                    .flat_map(|e| e.files.iter().map(|f| &f.path)),
            )
            .chain(m.links.iter().map(|l| &l.path))
            .chain(std::iter::once(&m.lib_dir))
            .chain((!m.library.is_empty()).then_some(&m.library))
        {
            safe_rel_path(p)?;
        }
        for l in &m.links {
            if l.target.contains(['/', '\\']) || l.target.is_empty() || l.target == ".." {
                return Err(PackError::Invalid(format!(
                    "bad link target {:?}",
                    l.target
                )));
            }
        }
        Ok(m)
    }
}

/// Create the manifest's links in `dir` (after the files are in place).
pub fn make_links(dir: &Path, m: &PackManifest) -> Result<()> {
    for l in &m.links {
        let path = dir.join(safe_rel_path(&l.path)?);
        let target = path.with_file_name(&l.target);
        if !target.is_file() {
            return Err(PackError::Invalid(format!(
                "link {} -> {}: no such file",
                l.path, l.target
            )));
        }
        let _ = std::fs::remove_file(&path);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&l.target, &path)?;
        #[cfg(not(unix))]
        std::fs::copy(&target, &path)?;
    }
    Ok(())
}

/// A part of a published pack archive (GitHub release assets are capped at 2 GiB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// `release.json` → `backends[target][variant]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendArtifact {
    /// Pack directory name (`torch-cu130-2.13.0`).
    pub name: String,
    pub torch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<u32>,
    /// sha256 and size of the whole archive (the parts concatenated).
    pub sha256: String,
    pub size: u64,
    pub parts: Vec<Part>,
    /// Bytes fetched from upstream (PyPI) on top of the archive.
    #[serde(default)]
    pub external_size: u64,
    /// Bytes on disk once installed.
    #[serde(default)]
    pub installed_size: u64,
}

/// Reject absolute paths, `..`, and anything that is not a plain relative path.
pub fn safe_rel_path(p: &str) -> Result<PathBuf> {
    let path = Path::new(p);
    if p.is_empty()
        || p.contains('\\')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(PackError::Invalid(format!("unsafe path in pack: {p:?}")));
    }
    Ok(path.to_path_buf())
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

fn check(what: &str, got: (String, u64), sha256: &str, size: u64) -> Result<()> {
    if got.1 != size {
        return Err(PackError::Size {
            what: what.into(),
            got: got.1,
            want: size,
        });
    }
    if !got.0.eq_ignore_ascii_case(sha256) {
        return Err(PackError::Checksum {
            what: what.into(),
            got: got.0,
            want: sha256.into(),
        });
    }
    Ok(())
}

/// Reads the parts of an archive back to back while hashing them.
struct Concat<'a> {
    files: Vec<PathBuf>,
    cur: Option<BufReader<File>>,
    hasher: Sha256,
    total: u64,
    /// Called with the archive bytes read so far.
    on_read: &'a mut dyn FnMut(u64),
}

impl Read for Concat<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.cur.is_none() {
                if self.files.is_empty() {
                    return Ok(0);
                }
                let f = self.files.remove(0);
                self.cur = Some(BufReader::with_capacity(1 << 20, File::open(f)?));
            }
            let n = self.cur.as_mut().map_or(Ok(0), |r| r.read(buf))?;
            if n == 0 {
                self.cur = None;
                continue;
            }
            self.hasher.update(&buf[..n]);
            self.total += n as u64;
            (self.on_read)(self.total);
            return Ok(n);
        }
    }
}

/// Unpack a pack archive (`.tar.zst`, possibly split in `parts`, in order) into
/// `staging`, then verify the whole archive's sha256 (when given) and every file
/// against `pack.json`. The archive has one top directory (the pack name); its
/// content lands directly in `staging`. Returns the manifest.
pub fn unpack_archive(
    parts: &[PathBuf],
    whole: Option<(&str, u64)>,
    staging: &Path,
) -> Result<PackManifest> {
    unpack_archive_progress(parts, whole, staging, |_, _| {})
}

/// [`unpack_archive`], calling `progress(read, total)` with the archive bytes read so
/// far and the parts' total size (an install from a local folder has no download to
/// show progress for, and a GPU pack takes a while to unpack).
pub fn unpack_archive_progress(
    parts: &[PathBuf],
    whole: Option<(&str, u64)>,
    staging: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PackManifest> {
    let size: u64 = parts
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut on_read = |read: u64| progress(read, size);
    let mut concat = Concat {
        files: parts.to_vec(),
        cur: None,
        hasher: Sha256::new(),
        total: 0,
        on_read: &mut on_read,
    };
    std::fs::create_dir_all(staging)?;
    let mut seen = BTreeSet::new();
    // Symlink entries (SONAME links): checked against pack.json, created by make_links.
    let mut symlinks: Vec<(String, String)> = Vec::new();
    {
        let dec = zstd::stream::read::Decoder::new(&mut concat)?;
        let mut tar = tar::Archive::new(dec);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let raw = entry.path()?.to_string_lossy().replace('\\', "/");
            // Strip the top directory.
            let rel = match raw.split_once('/') {
                Some((_, rest)) => rest.trim_end_matches('/').to_string(),
                None => String::new(),
            };
            let kind = entry.header().entry_type();
            if rel.is_empty() {
                continue;
            }
            let rel_path = safe_rel_path(&rel)?;
            if kind.is_dir() {
                std::fs::create_dir_all(staging.join(&rel_path))?;
                continue;
            }
            if kind.is_symlink() {
                let target = entry
                    .link_name()?
                    .map(|t| t.to_string_lossy().to_string())
                    .unwrap_or_default();
                if target.is_empty() || target.contains(['/', '\\']) || target == ".." {
                    return Err(PackError::Invalid(format!(
                        "{rel}: a link may only name a file next to it (got {target:?})"
                    )));
                }
                symlinks.push((rel, target));
                continue;
            }
            if !kind.is_file() {
                return Err(PackError::Invalid(format!(
                    "{rel}: only regular files and links are allowed in a pack"
                )));
            }
            let dest = staging.join(&rel_path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = BufWriter::with_capacity(1 << 20, File::create(&dest)?);
            std::io::copy(&mut entry, &mut out)?;
            out.flush()?;
            set_mode(&dest, entry.header().mode().unwrap_or(0o644))?;
            seen.insert(rel);
        }
        // Drain trailing padding so the whole-archive hash covers every byte.
        let mut dec = tar.into_inner();
        std::io::copy(&mut dec, &mut std::io::sink())?;
    }
    std::io::copy(&mut concat, &mut std::io::sink())?;
    if let Some((sha, size)) = whole {
        let got = (hex::encode(concat.hasher.clone().finalize()), concat.total);
        check("pack archive", got, sha, size)?;
    }
    let manifest = PackManifest::parse(&std::fs::read(staging.join(PACK_JSON))?)?;
    let listed: BTreeSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    for extra in seen.iter().filter(|p| *p != PACK_JSON) {
        if !listed.contains(extra) {
            return Err(PackError::Invalid(format!(
                "{extra} is in the archive but not in pack.json"
            )));
        }
    }
    for (path, target) in &symlinks {
        if !manifest
            .links
            .iter()
            .any(|l| &l.path == path && &l.target == target)
        {
            return Err(PackError::Invalid(format!(
                "{path} -> {target} is in the archive but not in pack.json's links"
            )));
        }
    }
    verify_files(staging, &manifest.files)?;
    make_links(staging, &manifest)?;
    Ok(manifest)
}

/// Check every listed file's size and sha256 under `dir`.
pub fn verify_files(dir: &Path, files: &[PackFile]) -> Result<()> {
    for f in files {
        let p = dir.join(safe_rel_path(&f.path)?);
        if !p.is_file() {
            return Err(PackError::Invalid(format!("{} is missing", f.path)));
        }
        check(&f.path, sha256_file(&p)?, &f.sha256, f.size)?;
    }
    Ok(())
}

/// Take the listed members out of an external archive (a wheel) into the pack.
pub fn unpack_external(archive: &Path, ext: &ExternalArchive, pack_dir: &Path) -> Result<()> {
    check(&ext.url, sha256_file(archive)?, &ext.sha256, ext.size)?;
    let mut zip = zip::ZipArchive::new(File::open(archive)?)
        .map_err(|e| PackError::Invalid(format!("{}: {e}", ext.name)))?;
    for f in &ext.files {
        let dest = pack_dir.join(safe_rel_path(&f.path)?);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        {
            let mut member = zip
                .by_name(&f.from)
                .map_err(|e| PackError::Invalid(format!("{} in {}: {e}", f.from, ext.name)))?;
            let mut out = BufWriter::with_capacity(1 << 20, File::create(&dest)?);
            std::io::copy(&mut member, &mut out)?;
            out.flush()?;
        }
        set_mode(&dest, 0o755)?;
        check(&f.path, sha256_file(&dest)?, &f.sha256, f.size)?;
    }
    Ok(())
}

/// Verify an installed pack: pack.json, every archive file and every external file.
pub fn verify_installed(dir: &Path) -> Result<PackManifest> {
    let m = PackManifest::parse(&std::fs::read(dir.join(PACK_JSON))?)?;
    verify_files(dir, &m.files)?;
    for e in &m.external {
        for f in &e.files {
            let p = dir.join(safe_rel_path(&f.path)?);
            if !p.is_file() {
                return Err(PackError::Invalid(format!(
                    "{} is missing (from {} {})",
                    f.path, e.name, e.version
                )));
            }
            check(&f.path, sha256_file(&p)?, &f.sha256, f.size)?;
        }
    }
    for l in &m.links {
        let p = dir.join(safe_rel_path(&l.path)?);
        if !p.is_file() {
            return Err(PackError::Invalid(format!("link {} is missing", l.path)));
        }
    }
    Ok(m)
}

/// Packs installed under `root` (directories `torch-*` with a readable pack.json),
/// newest torch first. Does not hash the files (see [`verify_installed`]).
pub fn installed(root: &Path) -> Vec<(PathBuf, PackManifest)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let dir = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("torch-") || !dir.is_dir() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(dir.join(PACK_JSON))
            && let Ok(m) = PackManifest::parse(&bytes)
        {
            out.push((dir, m));
        }
    }
    out.sort_by(|a, b| b.1.torch.cmp(&a.1.torch).then(a.1.name.cmp(&b.1.name)));
    out
}

/// Download `url` into `dest`, resuming a partial file, and check size + sha256.
/// `file://` URLs and plain paths are copied (offline installs, tests).
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    size: u64,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    if dest.is_file()
        && let Ok(got) = sha256_file(dest)
        && got.1 == size
        && got.0.eq_ignore_ascii_case(sha256)
    {
        progress(size, size);
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let local = url
        .strip_prefix("file://")
        .map(PathBuf::from)
        .or_else(|| (!url.contains("://")).then(|| PathBuf::from(url)));
    if let Some(src) = local {
        std::fs::copy(&src, dest)?;
        progress(size, size);
        return check(url, sha256_file(dest)?, sha256, size);
    }
    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > size {
        std::fs::remove_file(&part)?;
        have = 0;
    }
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut resp = req.send().await?.error_for_status()?;
    let resumed = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(true)
        .truncate(!resumed)
        .open(&part)?;
    let mut done = if resumed { have } else { 0 };
    let mut out = BufWriter::with_capacity(1 << 20, &mut file);
    while let Some(chunk) = resp.chunk().await? {
        out.write_all(&chunk)?;
        done += chunk.len() as u64;
        progress(done, size);
    }
    out.flush()?;
    drop(out);
    drop(file);
    let got = sha256_file(&part)?;
    if let Err(e) = check(url, got, sha256, size) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Swap a verified staging directory into place as `<root>/<name>`.
pub fn activate(staging: &Path, root: &Path, name: &str) -> Result<PathBuf> {
    safe_rel_path(name)?;
    let dest = root.join(name);
    if dest.exists() {
        let old = root.join(format!(".old-{name}"));
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&dest, &old)?;
        std::fs::rename(staging, &dest)?;
        let _ = std::fs::remove_dir_all(&old);
    } else {
        std::fs::rename(staging, &dest)?;
    }
    Ok(dest)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// Writes a pack archive the way `xtask torch-pack` does (pack.json first).
    fn write_pack(dir: &Path, files: &[(&str, &[u8])], extra: Option<(&str, &[u8])>) -> PathBuf {
        let manifest = PackManifest {
            format: PACK_FORMAT,
            name: "torch-cpu-2.13.0".into(),
            variant: "cpu".into(),
            torch: "2.13.0".into(),
            torch_build: String::new(),
            target: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            abi: 1,
            library: "libbunko_torch.so".into(),
            lib_dir: "lib".into(),
            bunko_version: "0.7.0".into(),
            requires: Requires::default(),
            files: files
                .iter()
                .map(|(p, b)| PackFile {
                    path: p.to_string(),
                    sha256: sha(b),
                    size: b.len() as u64,
                })
                .collect(),
            external: vec![],
            links: vec![PackLink {
                path: "lib/libc10.so.1".into(),
                target: "libc10.so".into(),
            }],
        };
        let out = dir.join("pack.tar.zst");
        let enc = zstd::stream::write::Encoder::new(File::create(&out).unwrap(), 3).unwrap();
        let mut tar = tar::Builder::new(enc);
        let mut add = |path: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, format!("torch-cpu-2.13.0/{path}"), data)
                .unwrap();
        };
        let json = serde_json::to_vec_pretty(&manifest).unwrap();
        add(PACK_JSON, &json);
        for (p, b) in files {
            add(p, b);
        }
        if let Some((p, b)) = extra {
            add(p, b);
        }
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        tar.append_link(&mut h, "torch-cpu-2.13.0/lib/libc10.so.1", "libc10.so")
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        out
    }

    #[test]
    fn unpack_and_verify_split_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = write_pack(
            tmp.path(),
            &[("lib/libc10.so", b"c10"), ("libbunko_torch.so", b"cdylib")],
            None,
        );
        let bytes = std::fs::read(&archive).unwrap();
        let (whole_sha, whole_size) = (sha(&bytes), bytes.len() as u64);
        // Split in three parts.
        let cut = [0, bytes.len() / 3, 2 * bytes.len() / 3, bytes.len()];
        let parts: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = tmp.path().join(format!("p{i}"));
                std::fs::write(&p, &bytes[cut[i]..cut[i + 1]]).unwrap();
                p
            })
            .collect();
        let staging = tmp.path().join("staging");
        let m = unpack_archive(&parts, Some((&whole_sha, whole_size)), &staging).unwrap();
        assert_eq!(m.name, "torch-cpu-2.13.0");
        // The loader's view (bunko_torch::abi::PackManifest) needs these keys.
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staging.join(PACK_JSON)).unwrap()).unwrap();
        for k in [
            "format", "variant", "torch", "abi", "os", "arch", "library", "lib_dir", "files",
        ] {
            assert!(v.get(k).is_some(), "pack.json lacks {k}");
        }
        assert_eq!(
            std::fs::read(staging.join("lib/libc10.so")).unwrap(),
            b"c10"
        );
        assert_eq!(
            std::fs::read(staging.join("lib/libc10.so.1")).unwrap(),
            b"c10"
        );
        let dest = activate(&staging, tmp.path(), &m.name).unwrap();
        assert_eq!(verify_installed(&dest).unwrap().files.len(), 2);
        assert_eq!(installed(tmp.path()).len(), 1);
        // Tampering is caught.
        std::fs::write(dest.join("lib/libc10.so"), b"evil").unwrap();
        assert!(verify_installed(&dest).is_err());
        // A wrong whole-archive hash is caught.
        let staging2 = tmp.path().join("staging2");
        assert!(matches!(
            unpack_archive(&parts, Some((&"0".repeat(64), whole_size)), &staging2),
            Err(PackError::Checksum { .. })
        ));
        // Progress: the bytes read across the parts, up to their total size.
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let staging3 = tmp.path().join("staging3");
        unpack_archive_progress(&parts, None, &staging3, |r, t| seen.push((r, t))).unwrap();
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|&(_, t)| t == whole_size));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(seen.last().unwrap().0, whole_size);
    }

    #[test]
    fn unlisted_and_unsafe_files_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = write_pack(tmp.path(), &[("lib/a.so", b"a")], Some(("lib/b.so", b"b")));
        let err = unpack_archive(&[archive], None, &tmp.path().join("s")).unwrap_err();
        assert!(err.to_string().contains("not in pack.json"), "{err}");
        for bad in ["../x", "/etc/passwd", "a/../../b", "", "a\\b"] {
            assert!(safe_rel_path(bad).is_err(), "{bad}");
        }
        assert!(safe_rel_path("lib/libc10.so").is_ok());
    }

    #[test]
    fn external_wheel_members() {
        let tmp = tempfile::tempdir().unwrap();
        let wheel = tmp.path().join("w.whl");
        {
            let mut z = zip::ZipWriter::new(File::create(&wheel).unwrap());
            z.start_file::<_, ()>(
                "nvidia/cu13/lib/libcudart.so.13",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            z.write_all(b"cudart").unwrap();
            z.finish().unwrap();
        }
        let (wsha, wsize) = sha256_file(&wheel).unwrap();
        let ext = ExternalArchive {
            name: "nvidia-cuda-runtime".into(),
            version: "13.0.96".into(),
            url: wheel.display().to_string(),
            sha256: wsha,
            size: wsize,
            license: "LicenseRef-NVIDIA-CUDA-EULA".into(),
            files: vec![ExternalFile {
                from: "nvidia/cu13/lib/libcudart.so.13".into(),
                path: "lib/libcudart.so.13".into(),
                sha256: sha(b"cudart"),
                size: 6,
            }],
        };
        let pack = tmp.path().join("pack");
        unpack_external(&wheel, &ext, &pack).unwrap();
        assert_eq!(
            std::fs::read(pack.join("lib/libcudart.so.13")).unwrap(),
            b"cudart"
        );
        let mut bad = ext.clone();
        bad.files[0].sha256 = "0".repeat(64);
        assert!(unpack_external(&wheel, &bad, &pack).is_err());
    }
}
