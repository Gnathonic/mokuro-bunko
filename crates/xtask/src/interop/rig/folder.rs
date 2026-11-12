//! The `folder` provider: a library folder on this disk that Bunko keeps as its storage
//! backend (`storage.library.backend: folder`), the way it keeps a cloud. The folder is
//! the library root, the part Drive's `mokuro-reader` folder plays, so its files sit
//! straight below it. Nothing serves it to the reader: the `shared` suite's Local
//! Folder specs drop it on the reader's page, which gives the page a handle to it in
//! place of the File System Access picker a rig cannot answer
//! (`interop/e2e/rig/local-folder.mjs`).

use super::super::server::{CLOUD_FOLDER, CloudLogin, folder_login};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Folder {
    /// The library folder.
    pub root: PathBuf,
}

impl Folder {
    /// `<work>/folder/Library`, created empty. Named unlike a cloud's library folder: a
    /// folder library's root may be called anything (Bunko reports it by
    /// `storage.library.folder` all the same). Its parent is the rig's own, so a
    /// scenario can take the whole folder away (`unplug`).
    pub fn create(work: &Path) -> Result<Folder> {
        let root = work.join("folder").join("Library");
        std::fs::create_dir_all(&root).with_context(|| root.display().to_string())?;
        Ok(Folder { root })
    }

    /// Put each `(path, bytes)` into the folder, folders created along the way. A path
    /// may name the cloud's library folder first (`mokuro-reader/Series/…`), as the
    /// cloud suites seed a cloud: that part is the folder itself.
    pub fn seed(&self, files: &[(String, Vec<u8>)]) -> Result<()> {
        for (path, data) in files {
            let dest = self.path(below_library(path));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, data).with_context(|| dest.display().to_string())?;
        }
        Ok(())
    }

    /// The file at `rel` (below the library folder) on disk.
    pub fn path(&self, rel: &str) -> PathBuf {
        rel.split('/').fold(self.root.clone(), |p, s| p.join(s))
    }

    /// The bytes at `rel`, or `None` when there is no such file.
    pub fn bytes(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(rel)).ok()
    }

    /// The server's sign-in: the credentials stub and `storage.library.path`.
    pub fn login(&self) -> CloudLogin {
        folder_login(&self.root)
    }

    /// Write `bytes` at `rel` as Chromium's File System Access does (the reader's Local
    /// Folder): the target exists at once (empty when new, its old bytes otherwise) and
    /// the new bytes go to `<name>.crswap` beside it. `during` runs while the write is
    /// in progress; then the swap file is renamed over the target, as `close()` does.
    pub fn swap_write(
        &self,
        rel: &str,
        bytes: &[u8],
        during: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let target = self.path(rel);
        let parent = target.parent().context("a file in the folder")?;
        std::fs::create_dir_all(parent)?;
        if !target.exists() {
            std::fs::write(&target, b"")?;
        }
        let name = target.file_name().context("a file name")?.to_string_lossy();
        let swap = parent.join(format!("{name}.crswap"));
        std::fs::write(&swap, bytes)?;
        let held = during();
        std::fs::rename(&swap, &target)?;
        held
    }

    /// Every `.cbz` and `.mokuro` below the folder, relative to it and sorted: the
    /// library's volumes and sidecars (no control files, nothing in Bunko's trash).
    pub fn volume_files(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        walk(&self.root, "", &mut out)?;
        out.retain(|r| r.ends_with(".cbz") || r.ends_with(".mokuro"));
        out.sort();
        Ok(out)
    }
}

/// A cloud path's part below the library folder: `mokuro-reader/Series/Vol 1.cbz` is
/// `Series/Vol 1.cbz`; a path without the folder is already that.
fn below_library(path: &str) -> &str {
    path.strip_prefix(CLOUD_FOLDER)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or(path)
}

/// Bunko's in-folder trash (`.mokuro-bunko-trash`), which a reader never lists.
const TRASH: &str = ".mokuro-bunko-trash";

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if rel.is_empty() && name == TRASH {
            continue;
        }
        let r = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if e.file_type()?.is_dir() {
            walk(&e.path(), &r, out)?;
        } else {
            out.push(r);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_seeded_without_the_cloud_folder_prefix() {
        let work = tempfile::tempdir().unwrap();
        let f = Folder::create(work.path()).unwrap();
        assert!(f.root.ends_with("folder/Library"));
        f.seed(&[
            ("mokuro-reader/Series/Vol 1.cbz".into(), b"one".to_vec()),
            ("Series/Vol 1.mokuro".into(), b"{}".to_vec()),
            (".mokuro-bunko.json".into(), b"{}".to_vec()),
        ])
        .unwrap();
        assert_eq!(f.bytes("Series/Vol 1.cbz").as_deref(), Some(&b"one"[..]));
        assert!(!f.root.join("mokuro-reader").exists());
        std::fs::create_dir_all(f.root.join(".mokuro-bunko-trash/x/Series")).unwrap();
        std::fs::write(
            f.root
                .join(".mokuro-bunko-trash/x/Series/Vol 9.cbz.bunko-trashed"),
            b"",
        )
        .unwrap();
        assert_eq!(
            f.volume_files().unwrap(),
            ["Series/Vol 1.cbz", "Series/Vol 1.mokuro"]
        );
        let login = f.login();
        assert_eq!(login.backend, "folder");
        assert_eq!(login.path.as_deref(), Some(f.root.as_path()));
    }

    #[test]
    fn a_swap_write_shows_its_bytes_only_after_the_swap() {
        let work = tempfile::tempdir().unwrap();
        let f = Folder::create(work.path()).unwrap();
        // New: an empty target while the bytes sit in the swap file.
        f.swap_write("S/Vol 1.cbz", b"new", || {
            assert_eq!(f.bytes("S/Vol 1.cbz").as_deref(), Some(&b""[..]));
            assert_eq!(f.bytes("S/Vol 1.cbz.crswap").as_deref(), Some(&b"new"[..]));
            Ok(())
        })
        .unwrap();
        assert_eq!(f.bytes("S/Vol 1.cbz").as_deref(), Some(&b"new"[..]));
        assert!(f.bytes("S/Vol 1.cbz.crswap").is_none());
        // Replaced: the old bytes stay until the swap, even when `during` fails.
        let err = f.swap_write("S/Vol 1.cbz", b"newer", || {
            assert_eq!(f.bytes("S/Vol 1.cbz").as_deref(), Some(&b"new"[..]));
            anyhow::bail!("seen mid-write")
        });
        assert!(err.is_err());
        assert_eq!(f.bytes("S/Vol 1.cbz").as_deref(), Some(&b"newer"[..]));
    }
}
