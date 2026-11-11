//! The local folder driver: a library in a folder on this computer (a local disk, a
//! mounted network share, a Docker bind mount), treated exactly like a cloud provider.
//! The folder chosen plays the role the `mokuro-reader` folder plays on Google Drive:
//! it is the library root, and Mokuro Reader's Local Folder reads the same folder.
//!
//! **Ids.** A pid is the path relative to the folder, `/`-separated, in its on-disk
//! spelling; `""` is the folder itself, reported under the configured root name
//! (`storage.library.folder`) whatever the directory is called. A move changes the pid
//! (`caps.path_ids`), as on WebDAV.
//!
//! **Versions.** Files: `version` = `cver` = `<mtime_ns>-<size>`; folders: `"dir"`.
//!
//! **Change feed.** A persisted snapshot diff (`<state_dir>/folder-snapshot.json`), as
//! WebDAV's; a filesystem watcher only wakes the feed early.
//!
//! This is the API skeleton: the types, the constructor and the capabilities are final,
//! the provider calls answer [`BErr::Unsupported`] until the driver lands.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangePage, Cursor, OpCtx, Pid, RNode, RemoteBody,
    ServerTime, UploadSource, UploadTarget,
};
use crate::governor::Limits;

/// The change-feed snapshot, in the state directory.
const SNAPSHOT_FILE: &str = "folder-snapshot.json";

/// How to open a folder library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderConfig {
    /// The library folder: absolute; the root Mokuro Reader's Local Folder uses.
    pub path: PathBuf,
    /// The name the root is reported by: `storage.library.folder` (`mokuro-reader`).
    pub root_name: String,
    /// Snapshot, memo, journal: `<base>/cloud/<store-id>/` (or the connect scratch dir).
    pub state_dir: PathBuf,
    /// Start the watcher (false for a connect Check).
    pub watch: bool,
}

/// What the connect probe found out about the folder's filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct FolderProbe {
    /// The folder exists and is a directory.
    pub reachable: bool,
    /// Bunko can create files in it (the write tests ran).
    pub writable: bool,
    /// `Aa` and `aA` name one entry (NTFS, APFS, vfat, CIFS, casefold).
    pub folds_case: bool,
    /// The filesystem stores names in another form than it was given (HFS+: NFD).
    pub rewrites_names: bool,
    /// A rename that refuses to replace works (`renameat2`, `renamex_np`, `MoveFileExW`).
    pub no_replace_rename: bool,
    /// `link()` works (the fallback for a create that must not replace).
    pub hard_links: bool,
    /// The modification-time resolution, in nanoseconds (0 when unknown).
    pub mtime_granularity_ns: i64,
    /// The folder is a mount point (its device differs from its parent's).
    pub mount_point: bool,
    /// The filesystem watcher started.
    pub watcher: bool,
}

/// A library in a local folder.
pub struct FolderBackend {
    cfg: FolderConfig,
    probe: FolderProbe,
}

impl std::fmt::Debug for FolderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderBackend")
            .field("path", &self.cfg.path)
            .field("root_name", &self.cfg.root_name)
            .field("probe", &self.probe)
            .finish_non_exhaustive()
    }
}

impl FolderBackend {
    /// Opens the folder library. Never fails because the folder or its drive is
    /// missing: the store opens and reports offline until it is back. Fails
    /// ([`BErr::Fatal`]) on a path that is not absolute, or a state directory and
    /// library folder that contain one another.
    pub async fn connect(cfg: FolderConfig) -> Result<FolderBackend, BErr> {
        if !cfg.path.is_absolute() {
            return Err(BErr::Fatal(format!(
                "the library folder must be a full path: {}",
                cfg.path.display()
            )));
        }
        Ok(FolderBackend {
            cfg,
            probe: FolderProbe::default(),
        })
    }

    /// What the connect probe found (all false when the folder was not reachable).
    pub fn probe(&self) -> FolderProbe {
        self.probe.clone()
    }
}

impl Backend for FolderBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            cas_create: self.probe.no_replace_rename || self.probe.hard_links,
            cas_overwrite: false,
            atomic_replace: true,
            server_copy: true,
            range: true,
            pinned_read: false,
            push: true,
            hash_kind: None,
            mtime_granularity_ns: 1_000_000,
            simple_upload_max: u64::MAX,
            chunk_multiple: 1,
            path_ids: true,
            duplicate_names: false,
        }
    }

    fn limits(&self) -> Limits {
        Limits::folder()
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        let id = account_id(&self.cfg.path);
        Ok(AccountInfo {
            label: id.clone(),
            id,
        })
    }

    async fn locate_roots(&self, _name: &str) -> Result<Vec<RNode>, BErr> {
        Err(BErr::Unsupported)
    }

    async fn create_root(&self, _name: &str) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        Err(BErr::Unsupported)
    }

    async fn list_all(
        &self,
        _roots: &[Pid],
        _sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn list_dir(
        &self,
        _dir: &str,
        _sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn find_named(
        &self,
        _parent: &str,
        _name: &str,
    ) -> Result<(Vec<RNode>, ServerTime), BErr> {
        Err(BErr::Unsupported)
    }

    async fn changes(&self, _c: &Cursor) -> Result<ChangePage, BErr> {
        Err(BErr::Unsupported)
    }

    /// Never fails: returns at `max` (the watcher wakes it early once it lands).
    async fn wait_change(&self, _c: &Cursor, max: Duration) -> Result<(), BErr> {
        tokio::time::sleep(max).await;
        Ok(())
    }

    async fn stat(&self, _pid: &str) -> Result<Option<RNode>, BErr> {
        Err(BErr::Unsupported)
    }

    async fn read(
        &self,
        _pid: &str,
        _pin: Option<&str>,
        _range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        Err(BErr::Unsupported)
    }

    async fn upload(
        &self,
        _to: UploadTarget<'_>,
        _src: UploadSource<'_>,
        _op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn mkdir(&self, _parent: &str, _name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn update(
        &self,
        _pid: &str,
        _parent: Option<&str>,
        _name: Option<&str>,
        _if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn copy(&self, _pid: &str, _parent: &str, _name: &str) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn trash(&self, _pid: &str, _if_cver: Option<&str>) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn delete_permanent(&self, _pid: &str) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        Err(BErr::Unsupported)
    }
}

/// The store id's and the credentials' account for a folder library: the absolute
/// path after `~` expansion, lexically normalised (no `.` or `..`, no trailing
/// separator), never resolved, so it does not change while the folder is away.
pub fn account_id(path: &Path) -> String {
    let expanded = bunko_core::storage::expand_user(path);
    let mut out = PathBuf::new();
    // Normal components pushed so far (a `..` pops only those, never the root).
    let mut depth = 0usize;
    for c in expanded.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(s) => {
                out.push(s);
                depth += 1;
            }
        }
    }
    out.to_string_lossy().into_owned()
}

/// For tests: the path the change-feed snapshot is kept at.
pub fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SNAPSHOT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn account_id_is_lexical() {
        assert_eq!(account_id(Path::new("/srv/manga")), "/srv/manga");
        assert_eq!(account_id(Path::new("/srv/manga/")), "/srv/manga");
        assert_eq!(account_id(Path::new("/srv/./x/../manga//")), "/srv/manga");
        assert_eq!(account_id(Path::new("/../srv")), "/srv");
        assert_eq!(account_id(Path::new("/")), "/");
        // Never resolved: a folder that does not exist keeps its id.
        assert_eq!(
            account_id(Path::new("/nonexistent.test/Manga")),
            "/nonexistent.test/Manga"
        );
    }

    #[cfg(windows)]
    #[test]
    fn account_id_is_lexical() {
        assert_eq!(account_id(Path::new(r"D:\Manga\")), r"D:\Manga");
        assert_eq!(account_id(Path::new(r"D:\x\..\Manga")), r"D:\Manga");
        assert_eq!(account_id(Path::new(r"D:\")), r"D:\");
        assert_eq!(
            account_id(Path::new(r"\\files.example.com\manga\")),
            r"\\files.example.com\manga\"
        );
    }

    #[test]
    fn snapshot_lives_in_the_state_dir() {
        assert_eq!(
            snapshot_path(Path::new("state")),
            Path::new("state").join("folder-snapshot.json")
        );
    }

    #[tokio::test]
    async fn connect_wants_a_full_path() {
        let cfg = FolderConfig {
            path: PathBuf::from("relative/manga"),
            root_name: "mokuro-reader".into(),
            state_dir: PathBuf::from("state"),
            watch: false,
        };
        assert!(matches!(
            FolderBackend::connect(cfg).await,
            Err(BErr::Fatal(_))
        ));
    }
}
