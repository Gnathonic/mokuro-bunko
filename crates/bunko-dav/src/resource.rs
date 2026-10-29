//! Resources of the DAV tree and their live properties (spec §8.1–8.2).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bunko_store::{Meta, fs};

use bunko_core::reader_tree::{self, Caller, PerUser};

use crate::paths::{self, READER_ROOT, Target};

/// A stat snapshot: everything the properties and validators need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// Inode-change time on Unix, creation time on Windows (Python `st_ctime`).
    pub ctime_secs: i64,
    pub is_dir: bool,
}

impl Stat {
    pub fn from_meta(meta: &Meta) -> Self {
        let (mtime_secs, mtime_nanos) = split_time(meta.modified().ok());
        let ctime_secs = meta
            .unix_ctime()
            .unwrap_or_else(|| split_time(meta.as_std().and_then(|m| m.created().ok())).0);
        Stat {
            size: meta.len(),
            mtime_secs,
            mtime_nanos,
            ctime_secs,
            is_dir: meta.is_dir(),
        }
    }

    /// Python's float `st_mtime` (`sec + nsec * 1e-9`).
    pub fn mtime_f64(&self) -> f64 {
        self.mtime_secs as f64 + self.mtime_nanos as f64 * 1e-9
    }

    /// `int(st_mtime)`, the second WsgiDAV compares HTTP dates against.
    pub fn mtime_int(&self) -> i64 {
        self.mtime_f64().trunc() as i64
    }

    /// File ETag value (unquoted), `f"{st_mtime:.6f}-{st_size}"`.
    pub fn file_etag(&self) -> String {
        format!("{:.6}-{}", self.mtime_f64(), self.size)
    }

    /// Folder ETag value, `f"{st_mtime:.6f}"`.
    pub fn folder_etag(&self) -> String {
        format!("{:.6}", self.mtime_f64())
    }

    pub fn last_modified_http(&self) -> String {
        http_date(self.mtime_int())
    }
}

fn split_time(t: Option<SystemTime>) -> (i64, u32) {
    match t {
        Some(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                // Before 1970: Python's float is still sec + frac.
                let d = e.duration();
                let secs = -(d.as_secs() as i64);
                if d.subsec_nanos() == 0 {
                    (secs, 0)
                } else {
                    (secs - 1, 1_000_000_000 - d.subsec_nanos())
                }
            }
        },
        None => (0, 0),
    }
}

/// RFC 1123 GMT (`Thu, 01 Oct 2026 18:10:08 GMT`) of a unix second.
pub fn http_date(secs: i64) -> String {
    let t = if secs >= 0 {
        UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
    };
    httpdate::fmt_http_date(t)
}

/// `%Y-%m-%dT%H:%M:%SZ` UTC (WsgiDAV `get_rfc3339_time`).
pub fn rfc3339(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        Err(_) => "1970-01-01T00:00:00Z".to_string(),
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Content type of a file by name (0.5.2 `get_content_type`).
pub fn content_type(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.ends_with(".json.gz") || lower.ends_with(".mokuro.gz") {
        return "application/gzip";
    }
    match paths::py_suffix(&lower) {
        ".cbz" => "application/vnd.comicbook+zip",
        ".cbr" => "application/vnd.comicbook-rar",
        ".zip" => "application/zip",
        ".gz" => "application/gzip",
        ".json" => "application/json",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Which side of the tree a file or folder lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Library,
    /// The caller's own tree (root files, `history/**`).
    Progress,
}

#[derive(Debug, Clone)]
pub enum Resource {
    /// `/` or `/mokuro-reader`: virtual collections with no physical folder.
    Virtual { path: String },
    Folder {
        path: String,
        phys: PathBuf,
        stat: Stat,
        kind: FileKind,
    },
    File {
        path: String,
        phys: PathBuf,
        stat: Stat,
        kind: FileKind,
    },
}

impl Resource {
    pub fn path(&self) -> &str {
        match self {
            Resource::Virtual { path }
            | Resource::Folder { path, .. }
            | Resource::File { path, .. } => path,
        }
    }

    pub fn is_collection(&self) -> bool {
        !matches!(self, Resource::File { .. })
    }

    pub fn phys(&self) -> Option<&Path> {
        match self {
            Resource::Virtual { .. } => None,
            Resource::Folder { phys, .. } | Resource::File { phys, .. } => Some(phys),
        }
    }

    pub fn kind(&self) -> Option<FileKind> {
        match self {
            Resource::Virtual { .. } => None,
            Resource::Folder { kind, .. } | Resource::File { kind, .. } => Some(*kind),
        }
    }

    pub fn stat(&self) -> Option<&Stat> {
        match self {
            Resource::Virtual { .. } => None,
            Resource::Folder { stat, .. } | Resource::File { stat, .. } => Some(stat),
        }
    }

    pub fn etag(&self) -> Option<String> {
        match self {
            Resource::Virtual { .. } => None,
            Resource::Folder { stat, .. } => Some(stat.folder_etag()),
            Resource::File { stat, .. } => Some(stat.file_etag()),
        }
    }

    /// `int(get_last_modified())`; virtual folders report *now*.
    pub fn last_modified(&self) -> i64 {
        self.stat().map(Stat::mtime_int).unwrap_or_else(now_secs)
    }

    pub fn href(&self) -> String {
        paths::href(self.path(), self.is_collection())
    }

    pub fn display_name(&self) -> String {
        match self {
            Resource::Virtual { path } => {
                if paths::normalize(path) == "/" {
                    "mokuro-bunko".to_string()
                } else {
                    paths::uri_name(path).to_string()
                }
            }
            Resource::Folder { phys, .. } | Resource::File { phys, .. } => paths::file_name(phys),
        }
    }
}

/// Filesystem roots the resolver maps into (canonical).
#[derive(Debug, Clone)]
pub struct Roots {
    pub library: PathBuf,
    pub users: PathBuf,
    pub history: crate::HistoryLimits,
}

impl Roots {
    /// Where `caller`'s per-user tree lives: `users/<username>`, or the library root
    /// for the cloud-root owner. `None` for a username that cannot be a directory.
    pub fn user_base(&self, caller: Caller<'_>) -> Option<PathBuf> {
        reader_tree::user_base(&self.library, &self.users, caller)
    }

    /// The physical path of the per-user `rel` (`goals.json`, `history/dev/x`) for
    /// `caller`, existing or not. A root file is its name under the base; anything in
    /// `history` must resolve (symlinks followed) inside the base's `history`.
    pub fn user_path(&self, caller: Caller<'_>, rel: &str) -> Option<PathBuf> {
        let base = self.user_base(caller)?;
        match reader_tree::per_user(rel)? {
            PerUser::File(name) => Some(base.join(name)),
            PerUser::Dir(dir) | PerUser::InDir { dir, .. } => {
                let resolved = paths::resolve_under(&base, rel.trim_matches('/'))?;
                resolved.starts_with(base.join(dir)).then_some(resolved)
            }
        }
    }

    /// Library-relative `/`-path of a physical path, or `None` when outside the library
    /// or one of the per-user names at its root (the cloud-root owner's own files): no
    /// library hook (uploads, OCR, uuids, audit 'library', nginx) ever sees those.
    pub fn library_rel(&self, phys: &Path) -> Option<String> {
        let rel = paths::relative_posix(phys, &self.library)?;
        let first = rel.split('/').next().unwrap_or("");
        (!reader_tree::is_per_user_root_name(first)).then_some(rel)
    }

    /// Resolve an existing resource (0.5.2 `get_resource_inst`). Blocking.
    pub fn lookup(&self, path: &str, caller: Option<Caller<'_>>) -> Option<Resource> {
        let norm = paths::normalize(path);
        match paths::classify(&norm) {
            Target::Root | Target::ReaderRoot => Some(Resource::Virtual { path: norm }),
            Target::Progress(rel) => {
                let phys = self.user_path(caller?, &rel)?;
                let meta = fs::metadata(&phys).ok()?;
                Some(Self::resource(
                    norm,
                    phys,
                    Stat::from_meta(&meta),
                    FileKind::Progress,
                ))
            }
            Target::Library(rel) => {
                let phys = self.library_member(&self.library, &rel)?;
                let meta = fs::metadata(&phys).ok()?;
                Some(Self::resource(
                    norm,
                    phys,
                    Stat::from_meta(&meta),
                    FileKind::Library,
                ))
            }
            Target::Inbox | Target::Other => None,
        }
    }

    /// `rel` resolved under the library folder `dir`, where it must stay, and never at
    /// one of the per-user names of the root (`S/../goals.json`, or a symlink to
    /// `history`, would otherwise reach the cloud-root owner's own files).
    pub(crate) fn library_member(&self, dir: &Path, rel: &str) -> Option<PathBuf> {
        let resolved = paths::resolve_under(dir, rel)?;
        (resolved.starts_with(&self.library) && self.library_rel(&resolved).is_some())
            .then_some(resolved)
    }

    fn resource(path: String, phys: PathBuf, stat: Stat, kind: FileKind) -> Resource {
        if stat.is_dir {
            Resource::Folder {
                path,
                phys,
                stat,
                kind,
            }
        } else {
            Resource::File {
                path,
                phys,
                stat,
                kind,
            }
        }
    }

    /// Is `phys` inside `caller`'s history tree (where a Progress folder's members must
    /// stay)?
    fn in_user_history(&self, caller: Caller<'_>, phys: &Path) -> bool {
        self.user_base(caller)
            .is_some_and(|base| phys.starts_with(base.join(reader_tree::HISTORY_DIR)))
    }

    /// The physical path a member `name` of the collection `parent` would have (0.5.2
    /// `create_empty_resource` / `create_collection`), with the file kind. `None` when the
    /// name cannot live there (traversal, symlink out, anonymous per-user file).
    pub fn member_path(
        &self,
        parent: &Resource,
        name: &str,
        caller: Option<Caller<'_>>,
    ) -> Option<(PathBuf, FileKind)> {
        match parent {
            Resource::Virtual { path } if paths::normalize(path) == format!("/{READER_ROOT}") => {
                if reader_tree::is_per_user_root_name(name) {
                    Some((self.user_path(caller?, name)?, FileKind::Progress))
                } else {
                    Some((self.library_member(&self.library, name)?, FileKind::Library))
                }
            }
            Resource::Folder {
                phys,
                kind: FileKind::Progress,
                ..
            } => {
                let resolved = paths::resolve_under(phys, name)?;
                self.in_user_history(caller?, &resolved)
                    .then_some((resolved, FileKind::Progress))
            }
            // The folder is inside the library; its member must stay there too.
            Resource::Folder { phys, .. } => self
                .library_member(phys, name)
                .map(|p| (p, FileKind::Library)),
            _ => None,
        }
    }

    /// Where a MOVE/COPY puts the member `name` of the collection `parent` (0.5.3
    /// `PathMapper.destination_to_physical`). [`Roots::member_path`], except that a library
    /// member is taken as written instead of resolved: on a case-insensitive filesystem
    /// resolving `Kingdom` can hand back the source's own `kingdom`, turning a rename that
    /// fixes the case into a rename onto itself. Where the literal path LEADS must still be
    /// inside the library (a COPY writes through a symlink sitting at the destination).
    /// Nothing moves into or inside the history tree.
    pub fn destination_path(
        &self,
        parent: &Resource,
        name: &str,
        caller: Option<Caller<'_>>,
    ) -> Option<(PathBuf, FileKind)> {
        let dir = match parent {
            Resource::Virtual { path } if paths::normalize(path) == format!("/{READER_ROOT}") => {
                if paths::is_per_user_name(name) {
                    return self.member_path(parent, name, caller);
                }
                if reader_tree::is_per_user_root_name(name) {
                    return None;
                }
                self.library.as_path()
            }
            Resource::Folder {
                kind: FileKind::Progress,
                ..
            } => return None,
            Resource::Folder { phys, .. } => phys.as_path(),
            _ => return None,
        };
        if matches!(name, "" | "." | "..") || name.contains('/') || name.contains('\0') {
            return None;
        }
        #[cfg(windows)]
        if name.contains('\\') {
            return None;
        }
        let candidate = dir.join(name);
        let rel = self.library_rel(&candidate)?;
        self.library_member(&self.library, &rel)?;
        Some((candidate, FileKind::Library))
    }

    /// Members of a collection: `(virtual path, resource)`, in listing order (spec §8.2).
    /// Blocking.
    pub fn members(&self, res: &Resource, caller: Option<Caller<'_>>) -> Vec<Resource> {
        match res {
            Resource::Virtual { path } => {
                let norm = paths::normalize(path);
                if norm == "/" {
                    return vec![Resource::Virtual {
                        path: format!("/{READER_ROOT}"),
                    }];
                }
                let mut out = Vec::new();
                if let Some(caller) = caller {
                    let names = reader_tree::PER_USER_FILES
                        .iter()
                        .chain(reader_tree::PER_USER_DIRS.iter());
                    for name in names {
                        if let Some(phys) = self.user_path(caller, name)
                            && let Ok(meta) = fs::metadata(&phys)
                        {
                            out.push(Self::resource(
                                format!("/{READER_ROOT}/{name}"),
                                phys,
                                Stat::from_meta(&meta),
                                FileKind::Progress,
                            ));
                        }
                    }
                }
                out.extend(self.list_dir(
                    &self.library,
                    &format!("/{READER_ROOT}"),
                    true,
                    FileKind::Library,
                ));
                out
            }
            Resource::Folder {
                path, phys, kind, ..
            } => self.list_dir(phys, &paths::normalize(path), false, *kind),
            Resource::File { .. } => Vec::new(),
        }
    }

    fn list_dir(
        &self,
        dir: &Path,
        vpath: &str,
        reader_root: bool,
        kind: FileKind,
    ) -> Vec<Resource> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<Resource> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // The per-user names at the library root are never the library's: a stray
            // copy, or the cloud-root owner's own files, which no one else may see.
            if reader_root && reader_tree::is_per_user_root_name(&name) {
                continue;
            }
            if paths::is_staging_name(&name) {
                continue;
            }
            let phys = entry.path();
            // Follow symlinks for the stat, as 0.5.2's DirEntry did; a dangling link is
            // skipped (0.5.2 listed it with no properties).
            let Ok(meta) = fs::metadata(&phys) else {
                continue;
            };
            let child = paths::join_uri(vpath, &name);
            out.push(Self::resource(child, phys, Stat::from_meta(&meta), kind));
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        out
    }

    /// Whether a listed sub-folder may be descended into for `Depth: infinity`: a symlink
    /// leading out of its tree (the library, or the caller's history) or into a loop is
    /// listed but not walked.
    pub fn walkable(
        &self,
        folder: &Resource,
        ancestors: &[PathBuf],
        caller: Option<Caller<'_>>,
    ) -> Option<PathBuf> {
        let phys = folder.phys()?;
        let canon = fs::canonicalize(phys).ok()?;
        let inside = match folder.kind() {
            Some(FileKind::Progress) => {
                let base = self.user_base(caller?)?.join(reader_tree::HISTORY_DIR);
                fs::canonicalize(base).is_ok_and(|b| canon.starts_with(b))
            }
            _ => canon.starts_with(&self.library) && self.library_rel(&canon).is_some(),
        };
        // Outside its tree, or the folder (or an ancestor of it) is already being
        // walked: a symlink loop.
        if !inside || ancestors.iter().any(|a| a.starts_with(&canon)) {
            return None;
        }
        Some(canon)
    }

    /// Bytes stored in `caller`'s history tree (small: one account's event logs).
    /// Blocking.
    pub fn history_bytes(&self, caller: Caller<'_>) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = fs::read_dir(dir) else {
                return 0;
            };
            let mut total = 0;
            for entry in entries.flatten() {
                let path = entry.path();
                match fs::symlink_metadata(&path) {
                    Ok(m) if m.is_dir() => total += walk(&path),
                    Ok(m) => total += m.len(),
                    Err(_) => {}
                }
            }
            total
        }
        self.user_base(caller)
            .map(|base| walk(&base.join(reader_tree::HISTORY_DIR)))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_formats_like_python() {
        let s = Stat {
            size: 1,
            mtime_secs: 1790878208,
            mtime_nanos: 881310123,
            ctime_secs: 0,
            is_dir: false,
        };
        assert_eq!(s.file_etag(), "1790878208.881310-1");
        assert_eq!(s.folder_etag(), "1790878208.881310");
        assert_eq!(http_date(1790878208), "Thu, 01 Oct 2026 18:10:08 GMT");
        assert_eq!(rfc3339(1790878208), "2026-10-01T18:10:08Z");
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
    fn per_user_paths_resolve_for_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for d in ["library", "users/alice", "elsewhere"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let roots = Roots {
            library: base.join("library"),
            users: base.join("users"),
            history: Default::default(),
        };
        let alice = Caller {
            username: "alice",
            owns_cloud_root: false,
        };
        let owner = Caller {
            owns_cloud_root: true,
            ..alice
        };
        assert_eq!(roots.user_base(alice), Some(base.join("users/alice")));
        assert_eq!(roots.user_base(owner), Some(base.join("library")));
        assert_eq!(
            roots.user_path(alice, "goals.json"),
            Some(base.join("users/alice/goals.json"))
        );
        assert_eq!(
            roots.user_path(owner, "history/d/x.events"),
            Some(base.join("library/history/d/x.events"))
        );
        assert_eq!(roots.user_path(alice, "history/../goals.json"), None);
        assert_eq!(roots.user_path(alice, "S/v.cbz"), None);
        // The owner's own files at the library root are no library path.
        assert_eq!(roots.library_rel(&base.join("library/history/d/x")), None);
        assert_eq!(roots.library_rel(&base.join("library/goals.json")), None);
        assert_eq!(
            roots
                .library_rel(&base.join("library/History/v.cbz"))
                .as_deref(),
            Some("History/v.cbz")
        );
        // A history that leads out of the account's tree is not the account's history.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("elsewhere"), base.join("users/alice/history"))
                .unwrap();
            assert_eq!(roots.user_path(alice, "history/d/x"), None);
            assert!(
                roots
                    .lookup("/mokuro-reader/history", Some(alice))
                    .is_none()
            );
        }
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("a.CBZ"), "application/vnd.comicbook+zip");
        assert_eq!(content_type("a.mokuro"), "application/octet-stream");
        assert_eq!(content_type("a.mokuro.gz"), "application/gzip");
        assert_eq!(content_type("series.json"), "application/json");
        assert_eq!(content_type("x.webp"), "image/webp");
        assert_eq!(content_type(".nocover"), "application/octet-stream");
    }
}
