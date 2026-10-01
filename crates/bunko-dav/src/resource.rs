//! Resources of the DAV tree and their live properties (spec §8.1–8.2).

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub fn from_meta(meta: &Metadata) -> Self {
        let (mtime_secs, mtime_nanos) = split_time(meta.modified().ok());
        #[cfg(unix)]
        let ctime_secs = {
            use std::os::unix::fs::MetadataExt;
            meta.ctime()
        };
        #[cfg(not(unix))]
        let ctime_secs = split_time(meta.created().ok()).0;
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

/// Which side of the tree a file lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Library,
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
}

impl Roots {
    /// `users/<username>/<name>`, or `None` for a username that cannot be a directory.
    pub fn user_file(&self, username: &str, name: &str) -> Option<PathBuf> {
        paths::safe_username(username).then(|| self.users.join(username).join(name))
    }

    /// Library-relative `/`-path of a physical path, or `None` when outside the library.
    pub fn library_rel(&self, phys: &Path) -> Option<String> {
        paths::relative_posix(phys, &self.library)
    }

    /// Resolve an existing resource (0.5.2 `get_resource_inst`). Blocking.
    pub fn lookup(&self, path: &str, username: Option<&str>) -> Option<Resource> {
        let norm = paths::normalize(path);
        match paths::classify(&norm) {
            Target::Root | Target::ReaderRoot => Some(Resource::Virtual { path: norm }),
            Target::Progress(name) => {
                let phys = self.user_file(username?, name)?;
                let meta = std::fs::metadata(&phys).ok()?;
                Some(Resource::File {
                    path: norm,
                    phys,
                    stat: Stat::from_meta(&meta),
                    kind: FileKind::Progress,
                })
            }
            Target::Library(rel) => {
                let phys = paths::resolve_under(&self.library, &rel)?;
                let meta = std::fs::metadata(&phys).ok()?;
                let stat = Stat::from_meta(&meta);
                if stat.is_dir {
                    Some(Resource::Folder {
                        path: norm,
                        phys,
                        stat,
                    })
                } else {
                    Some(Resource::File {
                        path: norm,
                        phys,
                        stat,
                        kind: FileKind::Library,
                    })
                }
            }
            Target::Inbox | Target::Other => None,
        }
    }

    /// The physical path a member `name` of the collection `parent` would have (0.5.2
    /// `create_empty_resource` / `create_collection`), with the file kind. `None` when the
    /// name cannot live there (traversal, symlink out, anonymous per-user file).
    pub fn member_path(
        &self,
        parent: &Resource,
        name: &str,
        username: Option<&str>,
    ) -> Option<(PathBuf, FileKind)> {
        match parent {
            Resource::Virtual { path } if paths::normalize(path) == format!("/{READER_ROOT}") => {
                if paths::is_per_user_name(name) {
                    Some((self.user_file(username?, name)?, FileKind::Progress))
                } else {
                    Some((
                        paths::resolve_under(&self.library, name)?,
                        FileKind::Library,
                    ))
                }
            }
            Resource::Folder { phys, .. } => {
                let resolved = paths::resolve_under(phys, name)?;
                // The folder is inside the library; its member must stay there too.
                resolved
                    .starts_with(&self.library)
                    .then_some((resolved, FileKind::Library))
            }
            _ => None,
        }
    }

    /// Members of a collection: `(virtual path, resource)`, in listing order (spec §8.2).
    /// Blocking.
    pub fn members(&self, res: &Resource, username: Option<&str>) -> Vec<Resource> {
        match res {
            Resource::Virtual { path } => {
                let norm = paths::normalize(path);
                if norm == "/" {
                    return vec![Resource::Virtual {
                        path: format!("/{READER_ROOT}"),
                    }];
                }
                let mut out = Vec::new();
                if let Some(user) = username {
                    for name in paths::PER_USER_FILES {
                        if let Some(phys) = self.user_file(user, name)
                            && let Ok(meta) = std::fs::metadata(&phys)
                        {
                            out.push(Resource::File {
                                path: format!("/{READER_ROOT}/{name}"),
                                phys,
                                stat: Stat::from_meta(&meta),
                                kind: FileKind::Progress,
                            });
                        }
                    }
                }
                out.extend(self.list_dir(&self.library, &format!("/{READER_ROOT}"), true));
                out
            }
            Resource::Folder { path, phys, .. } => {
                self.list_dir(phys, &paths::normalize(path), false)
            }
            Resource::File { .. } => Vec::new(),
        }
    }

    fn list_dir(&self, dir: &Path, vpath: &str, reader_root: bool) -> Vec<Resource> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<Resource> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if reader_root && paths::is_per_user_name(&name) {
                continue;
            }
            if paths::is_staging_name(&name) {
                continue;
            }
            let phys = entry.path();
            // Follow symlinks for the stat, as 0.5.2's DirEntry did; a dangling link is
            // skipped (0.5.2 listed it with no properties).
            let Ok(meta) = std::fs::metadata(&phys) else {
                continue;
            };
            let stat = Stat::from_meta(&meta);
            let child = paths::join_uri(vpath, &name);
            if stat.is_dir {
                out.push(Resource::Folder {
                    path: child,
                    phys,
                    stat,
                });
            } else {
                out.push(Resource::File {
                    path: child,
                    phys,
                    stat,
                    kind: FileKind::Library,
                });
            }
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        out
    }

    /// Whether a listed sub-folder may be descended into for `Depth: infinity`: a symlink
    /// leading out of the library (or into a loop) is listed but not walked.
    pub fn walkable(&self, folder: &Resource, ancestors: &[PathBuf]) -> Option<PathBuf> {
        let phys = folder.phys()?;
        let canon = std::fs::canonicalize(phys).ok()?;
        // Outside the library, or the folder (or an ancestor of it) is already being
        // walked: a symlink loop.
        if !canon.starts_with(&self.library) || ancestors.iter().any(|a| a.starts_with(&canon)) {
            return None;
        }
        Some(canon)
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
    fn content_types() {
        assert_eq!(content_type("a.CBZ"), "application/vnd.comicbook+zip");
        assert_eq!(content_type("a.mokuro"), "application/octet-stream");
        assert_eq!(content_type("a.mokuro.gz"), "application/gzip");
        assert_eq!(content_type("series.json"), "application/json");
        assert_eq!(content_type("x.webp"), "image/webp");
        assert_eq!(content_type(".nocover"), "application/octet-stream");
    }
}
