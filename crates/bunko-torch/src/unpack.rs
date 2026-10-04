//! `.pt2` zips are unpacked by us, once, into a cache directory and loaded in place.
//!
//! libtorch's own loader extracts a zip into the system temp directory on every load
//! (a RAM disk on some hosts; several hundred MB per package set) and leaves it behind
//! when the load fails or the process dies. Unpacking here puts the files where the
//! caller says (under `<storage>`), reuses them across loads, and removes partial or
//! failed unpacks.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::TorchError;

/// FNV-1a over the bytes: a stable, dependency-free cache key.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The cache directory a zip unpacks to: `<cache>/<name>-<hash of path, size, mtime>`.
pub fn unpacked_dir(zip: &Path, cache: &Path) -> Result<PathBuf, TorchError> {
    let meta =
        std::fs::metadata(zip).map_err(|e| TorchError::Load(format!("{}: {e}", zip.display())))?;
    let canon = std::fs::canonicalize(zip).unwrap_or_else(|_| zip.to_path_buf());
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let key = fnv1a(&[
        canon.to_string_lossy().as_bytes(),
        &meta.len().to_le_bytes(),
        &mtime.to_le_bytes(),
    ]);
    let name = zip
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "graph".into());
    Ok(cache.join(format!("{name}-{key:016x}")))
}

/// Removes a directory tree on drop unless disarmed.
struct Cleanup(Option<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Marks a finished unpack. `-2`: the archive's top `<role>/` folder is stripped, so
/// `data/aotinductor/` sits right in the directory (an unpack without it is redone).
const COMPLETE: &str = ".complete-2";

/// The single top folder every entry sits in, when there is one and it is not the
/// package's own `data/` (stripped on unpack).
fn common_top(names: &[PathBuf]) -> Option<std::ffi::OsString> {
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

/// The unpacked directory of `zip` under `cache`, unpacking it first if needed.
pub fn ensure_unpacked(zip: &Path, cache: &Path) -> Result<PathBuf, TorchError> {
    let dest = unpacked_dir(zip, cache)?;
    if dest.join(COMPLETE).is_file() {
        return Ok(dest);
    }
    // An unpack of the old layout (or a partial one): redone.
    let _ = std::fs::remove_dir_all(&dest);
    let err = |m: String| TorchError::Load(format!("unpacking {}: {m}", zip.display()));
    std::fs::create_dir_all(cache).map_err(|e| err(format!("{}: {e}", cache.display())))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = cache.join(format!(
        ".{}.part-{}-{nanos}",
        dest.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    let mut guard = Cleanup(Some(tmp.clone()));
    let file = std::fs::File::open(zip).map_err(|e| err(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| err(e.to_string()))?;
    let names: Vec<PathBuf> = (0..archive.len())
        .map(|i| {
            let e = archive.by_index_raw(i).map_err(|e| err(e.to_string()))?;
            e.enclosed_name()
                .ok_or_else(|| err(format!("unsafe entry name {:?}", e.name())))
        })
        .collect::<Result<_, _>>()?;
    let top = common_top(&names);
    for (i, rel) in names.iter().enumerate() {
        let mut entry = archive.by_index(i).map_err(|e| err(e.to_string()))?;
        let rel = match &top {
            Some(t) => rel.strip_prefix(t).unwrap_or(rel),
            None => rel.as_path(),
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = tmp.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| err(e.to_string()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| err(e.to_string()))?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| err(e.to_string()))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = entry.read(&mut buf).map_err(|e| err(e.to_string()))?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut w, &buf[..n]).map_err(|e| err(e.to_string()))?;
        }
    }
    std::fs::write(tmp.join(COMPLETE), b"").map_err(|e| err(e.to_string()))?;
    match std::fs::rename(&tmp, &dest) {
        Ok(()) => {
            guard.0 = None;
            Ok(dest)
        }
        // Another process finished the same unpack first: use theirs (ours is removed).
        Err(_) if dest.join(COMPLETE).is_file() => Ok(dest),
        Err(e) => Err(err(format!("{}: {e}", dest.display()))),
    }
}

/// Removes an unpacked directory whose load failed (so a bad package is not reused).
pub fn discard(dir: &Path) {
    if dir.join(COMPLETE).is_file() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        for (name, data) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut z, data).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn unpacks_once_and_reuses() {
        let tmp = std::env::temp_dir().join(format!("bt-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let z = tmp.join("vision.pt2");
        zip_with(
            &z,
            &[
                ("vision/data/aotinductor/model/a.so", b"x"),
                ("vision/archive_format", b"pt2"),
            ],
        );
        let cache = tmp.join("cache");
        let d = ensure_unpacked(&z, &cache).unwrap();
        assert!(d.join("data/aotinductor/model/a.so").is_file());
        assert!(d.join("archive_format").is_file() && !d.join("vision").exists());
        assert_eq!(ensure_unpacked(&z, &cache).unwrap(), d);
        // nothing but the finished directory is left in the cache
        let left: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
        discard(&d);
        assert!(!d.exists());
        // a zip-slip entry is refused and leaves nothing behind
        let bad = tmp.join("bad.pt2");
        zip_with(&bad, &[("ok/file", b"1"), ("../escape", b"2")]);
        assert!(ensure_unpacked(&bad, &cache).is_err());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        assert!(!tmp.join("escape").exists());
        // not a zip at all
        let junk = tmp.join("junk.pt2");
        std::fs::write(&junk, b"not a zip").unwrap();
        assert!(ensure_unpacked(&junk, &cache).is_err());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
