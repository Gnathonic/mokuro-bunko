//! Resources of the DAV tree and their live properties (spec §8.1–8.2).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bunko_store::{Meta, fs};

use bunko_core::reader_tree::{self, Caller, PerUser};
use bunko_core::sidecar_names::{self, SidecarClass};

use crate::paths::{self, READER_ROOT, Target};
use crate::personal::{self, PersonalFile};

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
    /// The caller's own correction layer at a library path (D27, [`crate::personal`]):
    /// an overlay under `users/<u>/edits`, or the cloud-root owner's physical file.
    Personal,
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

    /// The same resource as another [`FileKind`].
    pub fn with_kind(self, kind: FileKind) -> Resource {
        match self {
            Resource::Folder {
                path, phys, stat, ..
            } => Resource::Folder {
                path,
                phys,
                stat,
                kind,
            },
            Resource::File {
                path, phys, stat, ..
            } => Resource::File {
                path,
                phys,
                stat,
                kind,
            },
            other => other,
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

    /// The physical path of the per-user `rel` (`goals.json`, `history/dev/x`, in any
    /// case) for `caller`, existing or not. Built from the canonical per-user name, never
    /// from the raw `rel`: `History/x` is `history/x`. Under `users/<u>/` the per-user
    /// name is the entry its folder lists in any spelling (a `Goals.json` made there
    /// outside Bunko is the account's `goals.json`, read and written under its own
    /// spelling, on every host), decided from the listing; in the cloud-root owner's
    /// folder it is the canonical name (an entry spelled otherwise there is refused,
    /// [`crate::methods`]). Inside `history` every segment that names an existing entry
    /// takes its listed spelling (one device folder per device, whatever case the reader
    /// sends), and the result must resolve (symlinks followed) inside the base's
    /// `history`.
    pub fn user_path(&self, caller: Caller<'_>, rel: &str) -> Option<PathBuf> {
        let base = self.user_base(caller)?;
        match reader_tree::per_user(rel)? {
            PerUser::File(name) => Some(base.join(self.per_user_name(&base, name))),
            PerUser::Dir(dir) => {
                let dir = self.per_user_name(&base, dir);
                let resolved = paths::resolve_under(&base, &dir)?;
                resolved.starts_with(base.join(&dir)).then_some(resolved)
            }
            PerUser::InDir { dir, rest } => {
                let dir = self.per_user_name(&base, dir);
                let rest = crate::path_case::resolve_existing(&base.join(&dir), rest, false);
                let resolved = paths::resolve_under(&base, &format!("{dir}/{rest}"))?;
                resolved.starts_with(base.join(&dir)).then_some(resolved)
            }
        }
    }

    /// The spelling of the per-user root name `canonical` under `base`: `canonical`
    /// itself when `users/<u>/` lists it so (the file Bunko has always written; a stray
    /// `Goals.json` made beside it outside Bunko is left alone), else the listed entry
    /// that folds like it (the one a request reaches, when several do), else
    /// `canonical`. Always `canonical` in the library root (the cloud-root owner's
    /// base). From the listing: the same on every host. Blocking (one listing).
    fn per_user_name(&self, base: &Path, canonical: &str) -> String {
        if base == self.library {
            return canonical.to_string();
        }
        let Some((entries, _)) = crate::path_case::listed_groups(base, false) else {
            return canonical.to_string();
        };
        if entries.iter().any(|(n, _)| n == canonical) {
            return canonical.to_string();
        }
        crate::path_case::on_disk_name(base, canonical, false)
            .unwrap_or_else(|| canonical.to_string())
    }

    /// Is the per-user path `phys` (below `caller`'s base) listed exactly as spelled:
    /// its per-user name the one [`Self::per_user_name`] picks, and every segment below
    /// it the entry its folder lists ([`crate::path_case::reachable_as_spelled`])?
    /// Whether a per-user file exists, decided from the listings: the cloud-root owner's
    /// `goals.json` beside a listed `Goals.json` does not exist on any host.
    fn per_user_listed(&self, caller: Caller<'_>, phys: &Path) -> bool {
        let Some(base) = self.user_base(caller) else {
            return false;
        };
        let Some(rel) = paths::relative_posix(phys, &base) else {
            return false;
        };
        let (first, rest) = rel.split_once('/').unwrap_or((rel.as_str(), ""));
        if base == self.library {
            return crate::path_case::reachable_as_spelled(&base, &rel, false);
        }
        let Some(canonical) = reader_tree::canonical_per_user_rel(first) else {
            return false;
        };
        let canonical = canonical.trim_matches('/');
        if self.per_user_name(&base, canonical) != first
            || !crate::path_case::listed_groups(&base, false)
                .is_some_and(|(entries, _)| entries.iter().any(|(n, _)| n == first))
        {
            return false;
        }
        crate::path_case::reachable_as_spelled(&base.join(first), rest, false)
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
                let caller = caller?;
                let phys = self.user_path(caller, &rel)?;
                if !self.per_user_listed(caller, &phys) {
                    return None;
                }
                let meta = fs::metadata(&phys).ok()?;
                Some(Self::resource(
                    norm,
                    phys,
                    Stat::from_meta(&meta),
                    FileKind::Progress,
                ))
            }
            Target::Library(rel) => {
                if let Some(file) = self.personal(&rel) {
                    let phys = self.personal_path(&file.rel, caller?)?;
                    if !self.listed_as_spelled(&phys) {
                        return None;
                    }
                    let meta = fs::metadata(&phys).ok().filter(|m| !m.is_dir())?;
                    return Some(Self::resource(
                        norm,
                        phys,
                        Stat::from_meta(&meta),
                        FileKind::Personal,
                    ));
                }
                let phys = self.library_member(&self.library, &rel)?;
                if !self.listed_as_spelled(&phys) {
                    return None;
                }
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

    /// Does a library path exist as spelled: is each of its segments listed exactly so,
    /// as the entry a request reaches ([`crate::path_case::reachable_as_spelled`])? A
    /// path outside the library (an overlay, found through its listings already) is
    /// taken as it is. Never decided by opening the name: `Vol 2.cbz` beside a listed
    /// `vol 2.cbz` does not exist on APFS, NTFS and casefold either, and a hidden case
    /// variant's own files are not there for anyone. Blocking.
    fn listed_as_spelled(&self, phys: &Path) -> bool {
        match paths::relative_posix(phys, &self.library) {
            Some(rel) => crate::path_case::reachable_as_spelled(&self.library, &rel, true),
            None => true,
        }
    }

    /// The library path `rel` as a personal correction (D27): its name reads as an
    /// `edit`/`edit-<n>` layer and is no listed archive's primary (rule 4). Such a path
    /// is never the shared library file, whoever asks. `None` for any other path, and
    /// for one whose folder is not in the library. Blocking (lists the folder, only
    /// for such names).
    pub(crate) fn personal(&self, rel: &str) -> Option<PersonalFile> {
        let rel = rel.trim_matches('/');
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        personal::personal_name(name)?;
        let folder = self.library_member(&self.library, parent)?;
        self.personal_in(&folder, name, &personal::names_in(&folder))
    }

    /// The checks a PUT to the library path `rel` owes as a correction or curated
    /// layer (D27), `None` for any other file. Blocking (lists the folder, only for
    /// such names).
    pub(crate) fn layer_put_rule(&self, rel: &str) -> Option<Result<(), personal::LayerRefusal>> {
        let rel = rel.trim_matches('/');
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        sidecar_names::split_layer(name)?;
        let folder = self.library_member(&self.library, parent)?;
        personal::layer_put_rule(name, &personal::names_in(&folder))
    }

    /// [`Roots::personal`] for `name` in the library folder `folder` listing `names`.
    fn personal_in(&self, folder: &Path, name: &str, names: &[String]) -> Option<PersonalFile> {
        personal::personal_name(name)?;
        if let SidecarClass::Primary { .. } =
            sidecar_names::classify(name, &personal::stems_of(names))
        {
            return None;
        }
        let folder_rel = self.library_rel(folder)?;
        let rel = if folder_rel.is_empty() {
            name.to_string()
        } else {
            format!("{folder_rel}/{name}")
        };
        Some(PersonalFile { rel })
    }

    /// Where `caller`'s copy of the personal library file `rel` is: the physical file
    /// for the cloud-root owner, else the account's overlay.
    pub(crate) fn personal_path(&self, rel: &str, caller: Caller<'_>) -> Option<PathBuf> {
        if caller.owns_cloud_root {
            self.library_member(&self.library, rel)
        } else {
            personal::overlay_path(&self.library, &self.users, caller.username, rel)
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
        self.user_base(caller).is_some_and(|base| {
            phys.starts_with(base.join(self.per_user_name(&base, reader_tree::HISTORY_DIR)))
        })
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
                } else if let Some(file) = self.personal_member(&self.library, name) {
                    Some((self.personal_path(&file.rel, caller?)?, FileKind::Personal))
                } else {
                    Some((self.library_member(&self.library, name)?, FileKind::Library))
                }
            }
            Resource::Folder {
                phys,
                kind: FileKind::Progress,
                ..
            } => {
                // An existing member in any spelling is that member (one device folder
                // per device on every host).
                let name = crate::path_case::on_disk_name(phys, name, false)
                    .unwrap_or_else(|| name.to_string());
                let resolved = paths::resolve_under(phys, &name)?;
                self.in_user_history(caller?, &resolved)
                    .then_some((resolved, FileKind::Progress))
            }
            // The folder is inside the library; its member must stay there too.
            Resource::Folder { phys, .. } => match self.personal_member(phys, name) {
                Some(file) => Some((self.personal_path(&file.rel, caller?)?, FileKind::Personal)),
                None => self
                    .library_member(phys, name)
                    .map(|p| (p, FileKind::Library)),
            },
            _ => None,
        }
    }

    /// The member `name` of the library folder `folder` as a personal correction.
    fn personal_member(&self, folder: &Path, name: &str) -> Option<PersonalFile> {
        personal::personal_name(name)?;
        if name.contains(['/', '\\', '\0']) || matches!(name, "" | "." | "..") {
            return None;
        }
        self.personal_in(folder, name, &personal::names_in(folder))
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
        // A correction moves to the caller's copy, archive or not (X10: the reader
        // moves layers before the archive).
        if let Some(file) = self.personal_member(dir, name) {
            return Some((self.personal_path(&file.rel, caller?)?, FileKind::Personal));
        }
        let candidate = dir.join(name);
        let rel = self.library_rel(&candidate)?;
        self.library_member(&self.library, &rel)?;
        Some((candidate, FileKind::Library))
    }

    /// Members of a collection: `(virtual path, resource)`, in listing order (spec §8.2).
    /// Blocking.
    pub fn members(&self, res: &Resource, caller: Option<Caller<'_>>) -> Vec<Resource> {
        self.members_hiding(res, caller, None)
    }

    /// [`Roots::members`]; the physical personal files `caller` may not see go into
    /// `hidden` (the shared listing keeps them for the cloud-root owner).
    pub fn members_hiding(
        &self,
        res: &Resource,
        caller: Option<Caller<'_>>,
        hidden: Option<&mut Vec<Resource>>,
    ) -> Vec<Resource> {
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
                out.extend(self.library_dir(
                    &self.library,
                    &format!("/{READER_ROOT}"),
                    true,
                    caller,
                    hidden,
                ));
                out
            }
            Resource::Folder {
                path,
                phys,
                kind: FileKind::Library,
                ..
            } => self.library_dir(phys, &paths::normalize(path), false, caller, hidden),
            Resource::Folder {
                path, phys, kind, ..
            } => self.list_dir(phys, &paths::normalize(path), false, *kind),
            Resource::File { .. } => Vec::new(),
        }
    }

    /// A library folder's members as `caller` sees them: a correction beside its
    /// archive is the owner's physical file or, for anyone else, their overlay.
    fn library_dir(
        &self,
        dir: &Path,
        vpath: &str,
        reader_root: bool,
        caller: Option<Caller<'_>>,
        mut hidden: Option<&mut Vec<Resource>>,
    ) -> Vec<Resource> {
        let listed = self.list_dir(dir, vpath, reader_root, FileKind::Library);
        // Files only: a folder named like an archive defines no stem (rule 2), as
        // `personal::names_in` reads it for GET and auth.
        let names: Vec<String> = listed
            .iter()
            .filter(|r| !r.is_collection())
            .map(|r| paths::uri_name(r.path()).to_string())
            .collect();
        let stems = personal::stems_of(&names);
        let owner = caller.is_some_and(|c| c.owns_cloud_root);
        let mut out = Vec::with_capacity(listed.len());
        for res in listed {
            let name = paths::uri_name(res.path());
            if res.is_collection() || personal::personal_name(name).is_none() {
                out.push(res);
                continue;
            }
            match sidecar_names::classify(name, &stems) {
                SidecarClass::Primary { .. } => out.push(res),
                SidecarClass::Layer { .. } => {
                    let res = res.with_kind(FileKind::Personal);
                    if owner {
                        out.push(res);
                    } else if let Some(h) = hidden.as_deref_mut() {
                        h.push(res);
                    }
                }
                // Beside no archive: nobody's to see.
                SidecarClass::Orphan => {}
            }
        }
        if let Some(caller) = caller.filter(|c| !c.owns_cloud_root)
            && let Some(folder_rel) = self.library_rel(dir)
        {
            out.extend(self.overlays_in(caller, &folder_rel, vpath, dir));
            out.sort_by(|a, b| a.path().cmp(b.path()));
        }
        out
    }

    /// `caller`'s overlays of the library folder `folder_rel` (virtual folder `vpath`,
    /// physical `folder`) that rule v1, over the folder's whole listing, gives to an
    /// archive a request reaches: never a hidden case variant's (named by its exact
    /// stem), nor one beside no archive.
    fn overlays_in(
        &self,
        caller: Caller<'_>,
        folder_rel: &str,
        vpath: &str,
        folder: &Path,
    ) -> Vec<Resource> {
        let Some(dir) =
            personal::overlay_path(&self.library, &self.users, caller.username, folder_rel)
        else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let overlays: Vec<(String, PathBuf)> = entries
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(name, _)| personal::personal_name(name).is_some())
            .collect();
        if overlays.is_empty() {
            return Vec::new();
        }
        let (listed, hidden) = crate::path_case::listed_with_hidden(folder);
        let files: Vec<&str> = listed
            .iter()
            .filter(|(n, d)| !d && !paths::is_staging_name(n))
            .map(|(n, _)| n.as_str())
            .collect();
        let archives: Vec<&str> = files
            .iter()
            .copied()
            .filter(|n| sidecar_names::cbz_stem(n).is_some() && !hidden.contains(*n))
            .collect();
        let mut names = files.clone();
        names.extend(overlays.iter().map(|(n, _)| n.as_str()));
        let rule = sidecar_names::Folder::new(&names);
        let shown: std::collections::HashSet<&str> = archives
            .iter()
            .flat_map(|archive| rule.volume_files(archive))
            .filter_map(|(n, file)| {
                matches!(file, sidecar_names::VolumeFile::Layer { .. }).then_some(n)
            })
            .collect();
        let mut out = Vec::new();
        for (name, phys) in &overlays {
            if !shown.contains(name.as_str()) {
                continue;
            }
            if let Ok(meta) = fs::metadata(phys)
                && !meta.is_dir()
            {
                out.push(Self::resource(
                    paths::join_uri(vpath, name),
                    phys.clone(),
                    Stat::from_meta(&meta),
                    FileKind::Personal,
                ));
            }
        }
        out
    }

    /// Every overlay of `caller` below the library folder `folder_rel` (`""`: the
    /// whole library) that a listing shows: each beside its archive, in path order.
    /// For a `Depth: infinity` answer, whose shared part is the anonymous cache.
    /// Blocking.
    pub fn overlays_under(&self, caller: Caller<'_>, folder_rel: &str) -> Vec<Resource> {
        let mut out = Vec::new();
        let mut folders = std::collections::BTreeSet::new();
        let mut stack = vec![folder_rel.trim_matches('/').to_string()];
        while let Some(rel) = stack.pop() {
            let Some(dir) =
                personal::overlay_path(&self.library, &self.users, caller.username, &rel)
            else {
                continue;
            };
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut has_files = false;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(if rel.is_empty() {
                        name
                    } else {
                        format!("{rel}/{name}")
                    }),
                    Ok(_) => has_files |= personal::personal_name(&name).is_some(),
                    Err(_) => {}
                }
            }
            if !has_files {
                continue;
            }
            // The overlay folder's library folder, in the spelling the listing shows (a
            // folder respelled outside Bunko or in the cloud included; a shadowed
            // folder's overlays are its own, never shown), each listed once.
            let Some(library_rel) = crate::path_case::listed_entry(&self.library, &rel) else {
                continue;
            };
            folders.insert(library_rel);
        }
        for rel in folders {
            let Some(folder) = self.library_member(&self.library, &rel) else {
                continue;
            };
            let vpath = if rel.is_empty() {
                format!("/{READER_ROOT}")
            } else {
                format!("/{READER_ROOT}/{rel}")
            };
            out.extend(self.overlays_in(caller, &rel, &vpath, &folder));
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        out
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
        let entries: Vec<_> = entries.flatten().collect();
        // Another spelling of a listed name (only a case-sensitive disk or a remote
        // store holds one) is shadowed, and so are the files rule v1 gives to a
        // shadowed archive: no request reaches them, so they are not listed.
        let names: Vec<(String, bool)> = entries
            .iter()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                Some((name, e.file_type().is_ok_and(|t| t.is_dir())))
            })
            .filter(|(n, _)| !(reader_root && reader_tree::is_per_user_root_name(n)))
            .collect();
        let shadowed = crate::path_case::hidden(dir, &names);
        let mut out: Vec<Resource> = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            // The per-user names at the library root (in any spelling) are never the
            // library's: a stray copy, or the cloud-root owner's own files, which no one
            // else may see.
            if reader_root && reader_tree::is_per_user_root_name(&name) {
                continue;
            }
            if shadowed.contains(&name) {
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
        // A per-user name in any case is reserved: never a library path, and the
        // physical path is built from the canonical name.
        assert_eq!(roots.library_rel(&base.join("library/History/v.cbz")), None);
        assert_eq!(
            roots.user_path(alice, "Goals.json"),
            Some(base.join("users/alice/goals.json"))
        );
        assert_eq!(
            roots.user_path(owner, "History/d/x.events"),
            Some(base.join("library/history/d/x.events"))
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

    /// D27 with X1: a correction is `edit`/`edit-<n>` exactly as written, and a dotted
    /// volume's own primary is never one (rule 4).
    #[test]
    #[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
    fn personal_names_resolve_per_caller() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("library/S")).unwrap();
        std::fs::create_dir_all(base.join("users")).unwrap();
        for n in ["Vol.cbz", "D.edit.cbz"] {
            std::fs::write(base.join("library/S").join(n), b"x").unwrap();
        }
        let roots = Roots {
            library: base.join("library"),
            users: base.join("users"),
            history: Default::default(),
        };
        let rel = |p: &str| roots.personal(p).map(|f| f.rel);
        assert_eq!(
            rel("S/Vol.edit.mokuro").as_deref(),
            Some("S/Vol.edit.mokuro")
        );
        assert_eq!(
            rel("S/Vol.edit-2.mokuro").as_deref(),
            Some("S/Vol.edit-2.mokuro")
        );
        // No archive, or no folder: still the caller's (never the shared file), only
        // unlisted.
        assert_eq!(
            rel("S/Nope.edit.mokuro").as_deref(),
            Some("S/Nope.edit.mokuro")
        );
        assert_eq!(rel("S/Vol.EDIT.mokuro"), None);
        assert_eq!(rel("S/Vol.edited.mokuro"), None);
        assert_eq!(rel("S/D.edit.mokuro"), None);
        assert_eq!(
            rel("Missing/Vol.edit.mokuro").as_deref(),
            Some("Missing/Vol.edit.mokuro")
        );
        let alice = Caller {
            username: "alice",
            owns_cloud_root: false,
        };
        let owner = Caller {
            owns_cloud_root: true,
            ..alice
        };
        assert_eq!(
            roots.personal_path("S/Vol.edit.mokuro", alice),
            Some(base.join("users/alice/edits/S/Vol.edit.mokuro"))
        );
        assert_eq!(
            roots.personal_path("S/Vol.edit.mokuro", owner),
            Some(base.join("library/S/Vol.edit.mokuro"))
        );
        let nobody = Caller {
            username: "..",
            owns_cloud_root: false,
        };
        assert_eq!(roots.personal_path("S/Vol.edit.mokuro", nobody), None);
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
