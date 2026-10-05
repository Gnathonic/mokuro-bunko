//! Deterministic `.tar.gz` / `.zip` writers for a staged directory.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Files under `dir`, sorted, as paths relative to `dir`.
pub fn list_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(dir)?.to_path_buf());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether a staged file gets the executable bit.
pub fn is_executable(rel: &Path) -> bool {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    name == crate::names::BIN
        || name == crate::names::TRAY_BIN
        || name.ends_with(".exe")
        || name.ends_with(".sh")
        || name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.contains(".so.")
}

fn unix_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `out` = gzip'd tar of `stage` with every entry under `top/`.
pub fn write_tar_gz(stage: &Path, top: &str, out: &Path, mtime: u64) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    dir.set_mtime(mtime);
    dir.set_cksum();
    tar.append_data(&mut dir, format!("{top}/"), std::io::empty())?;
    // Files staged as hard links of an earlier file (the macOS bundle's copy of the
    // CLI) become tar hard-link entries: no second copy in the download. The earlier
    // file sorts first, so readers that take the first entry by name (bunko-update's
    // extract_binary) get the real file.
    let mut seen: std::collections::HashMap<(u64, u64), String> = Default::default();
    for rel in list_files(stage)? {
        let path = stage.join(&rel);
        let meta = std::fs::metadata(&path)?;
        let name = format!("{top}/{}", unix_path(&rel));
        if let Some(key) = inode(&meta) {
            if let Some(first) = seen.get(&key) {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Link);
                h.set_size(0);
                h.set_mode(if is_executable(&rel) { 0o755 } else { 0o644 });
                h.set_mtime(mtime);
                h.set_uid(0);
                h.set_gid(0);
                tar.append_link(&mut h, &name, first)?;
                continue;
            }
            seen.insert(key, name.clone());
        }
        let mut h = tar::Header::new_gnu();
        h.set_size(meta.len());
        h.set_mode(if is_executable(&rel) { 0o755 } else { 0o644 });
        h.set_mtime(mtime);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        tar.append_data(&mut h, &name, std::fs::File::open(&path)?)?;
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(())
}

/// `(device, inode)` of a file with more than one link (Unix), else None.
#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn inode(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// `out` = zip of `stage` with every entry under `top/`.
pub fn write_zip(stage: &Path, top: &str, out: &Path) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    for rel in list_files(stage)? {
        let mode = if is_executable(&rel) { 0o755 } else { 0o644 };
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9))
            .large_file(std::fs::metadata(stage.join(&rel))?.len() >= u32::MAX as u64)
            .unix_permissions(mode);
        zip.start_file(format!("{top}/{}", unix_path(&rel)), opts)?;
        std::io::copy(&mut std::fs::File::open(stage.join(&rel))?, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}

/// Extract every regular file of a `.tar.gz` or `.zip` into `dest`, dropping the top
/// directory, and return the relative paths written.
pub fn extract_flat(archive: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest)?;
    let mut written = Vec::new();
    let strip = |p: &Path| -> Option<PathBuf> {
        let mut c = p.components();
        c.next()?;
        let rest: PathBuf = c.collect();
        // Never write outside `dest`.
        if rest.as_os_str().is_empty()
            || rest
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            None
        } else {
            Some(rest)
        }
    };
    let name = archive.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.is_dir() {
                continue;
            }
            let Some(rel) = f.enclosed_name().as_deref().and_then(strip) else {
                continue;
            };
            let out = dest.join(&rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::io::copy(&mut f, &mut std::fs::File::create(&out)?)?;
            set_mode(&out, f.unix_mode().unwrap_or(0o644))?;
            written.push(rel);
        }
    } else {
        let mut tar =
            tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?));
        for entry in tar.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let Some(rel) = strip(&entry.path()?) else {
                continue;
            };
            let out = dest.join(&rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mode = entry.header().mode().unwrap_or(0o644);
            std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
            set_mode(&out, mode)?;
            written.push(rel);
        }
    }
    written.sort();
    Ok(written)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_and_zip_roundtrip_with_updater() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        std::fs::create_dir_all(stage.join("sub")).unwrap();
        std::fs::write(stage.join("mokuro-bunko"), b"bin").unwrap();
        std::fs::write(stage.join("mokuro-bunko.exe"), b"exe").unwrap();
        std::fs::write(stage.join("sub/README.md"), b"hi").unwrap();

        let tgz = dir.path().join("a.tar.gz");
        write_tar_gz(&stage, "top", &tgz, 0).unwrap();
        let zip = dir.path().join("a.zip");
        write_zip(&stage, "top", &zip).unwrap();

        // The updater must find the executable inside both archives.
        let out = dir.path().join("out");
        bunko_update::extract_binary(&tgz, "https://x/a.tar.gz", "mokuro-bunko", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"bin");
        bunko_update::extract_binary(&zip, "https://x/a.zip", "mokuro-bunko.exe", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"exe");

        for a in [&tgz, &zip] {
            let x = dir
                .path()
                .join(format!("x-{}", a.extension().unwrap().to_string_lossy()));
            let files = extract_flat(a, &x).unwrap();
            assert_eq!(files.len(), 3, "{files:?}");
            assert_eq!(std::fs::read(x.join("sub/README.md")).unwrap(), b"hi");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(x.join("mokuro-bunko"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o755
                );
            }
        }
    }

    /// The macOS layout: the CLI at the top and, hard-linked, inside the .app. The
    /// archive holds it once; the updater and `tar -x` both get the real file.
    #[cfg(unix)]
    #[test]
    fn bundle_hard_link_is_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let macos = stage.join("mokuro-bunko.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7919 % 251) as u8).collect();
        std::fs::write(stage.join("mokuro-bunko"), &big).unwrap();
        std::fs::hard_link(stage.join("mokuro-bunko"), macos.join("mokuro-bunko")).unwrap();
        std::fs::write(macos.join("mokuro-bunko-tray"), b"tray").unwrap();
        let tgz = dir.path().join("m.tar.gz");
        write_tar_gz(&stage, "top", &tgz, 0).unwrap();

        let mut kinds = Vec::new();
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(
            std::fs::File::open(&tgz).unwrap(),
        ));
        for e in t.entries().unwrap() {
            let e = e.unwrap();
            kinds.push((
                e.path().unwrap().to_string_lossy().to_string(),
                e.header().entry_type(),
                e.size(),
            ));
        }
        assert_eq!(kinds[1].0, "top/mokuro-bunko");
        assert_eq!(kinds[1].2, big.len() as u64);
        let link = kinds
            .iter()
            .find(|k| k.0 == "top/mokuro-bunko.app/Contents/MacOS/mokuro-bunko")
            .unwrap();
        assert_eq!(link.1, tar::EntryType::Link);
        assert_eq!(link.2, 0);

        let out = dir.path().join("out");
        bunko_update::extract_binary(&tgz, "https://x/m.tar.gz", "mokuro-bunko", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), big);
        // A real tar recreates the link.
        let x = dir.path().join("x");
        std::fs::create_dir_all(&x).unwrap();
        let ok = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&tgz)
            .arg("-C")
            .arg(&x)
            .status()
            .unwrap();
        assert!(ok.success());
        assert_eq!(
            std::fs::read(x.join("top/mokuro-bunko.app/Contents/MacOS/mokuro-bunko")).unwrap(),
            big
        );
    }
}
