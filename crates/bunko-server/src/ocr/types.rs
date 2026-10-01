//! Small shared types of the OCR orchestrator: a job, archive stamps, the library facts
//! the scheduler reads through the metadata layer, and path helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `"local"`: this server's own hardware, by the name its numbers are filed under.
pub const LOCAL: &str = "local";
/// What people call this server's own hardware.
pub const LOCAL_DISPLAY: &str = "this server";

/// One unit of queue work: a volume (library-relative posix path of its `.cbz`) and the
/// generation row (by immutable id) it is owed. `upgrade` marks a generation-upgrade
/// job of the primary row (generate, then swap in as the bare `.mokuro`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Job {
    pub rel: Arc<str>,
    pub gid: Arc<str>,
    pub upgrade: bool,
}

impl Job {
    pub fn new(rel: &str, gid: &str) -> Job {
        Job { rel: Arc::from(rel), gid: Arc::from(gid), upgrade: false }
    }

    pub fn upgrade(rel: &str, gid: &str) -> Job {
        Job { rel: Arc::from(rel), gid: Arc::from(gid), upgrade: true }
    }

    /// `path.parent.relative_to(library).as_posix()`: `"."` for a root-level volume.
    pub fn series(&self) -> &str {
        series_of(&self.rel)
    }

    /// The archive's stem.
    pub fn volume(&self) -> &str {
        stem_of(&self.rel)
    }

    pub fn file_name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }

    pub fn path(&self, library: &Path) -> PathBuf {
        library.join(&*self.rel)
    }
}

pub fn series_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => ".",
    }
}

/// Python `Path(name).stem` of the last component.
pub fn stem_of(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// The library-relative posix path of `path`, or None outside the library.
pub fn rel_of(library: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(library).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.is_empty() || parts.iter().any(|p| p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

/// `(st_size, st_mtime_ns)` of an archive: what a claim pins.
pub type Stamp = (u64, i128);

pub fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((meta.len(), mtime_ns(&meta)))
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

/// What the scheduler asks of the metadata layer (bunko-library over the database).
/// Every method answers something: an error reads as "nothing known".
pub trait LibraryFacts: Send + Sync {
    /// `missing_pages_now`: pages the volume's primary `.mokuro` names that the archive
    /// lacks (0 when it has none, or on any error).
    fn missing_pages(&self, cbz: &Path) -> i64;
    /// `cached_page_count`: the metadata cache's page count, if compiled.
    fn page_count(&self, _cbz: &Path) -> Option<i64> {
        None
    }
    /// A sidecar was installed (or swapped) beside `cbz`: recompile its series.
    fn sidecar_installed(&self, _cbz: &Path) {}
    /// Volumes still waiting for a cover thumbnail (the queue page's count).
    fn pending_thumbnails(&self) -> i64 {
        0
    }
}

/// Facts read straight from the files (no metadata cache): what tests and a server
/// without the metadata service use.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileFacts;

impl LibraryFacts for FileFacts {
    fn missing_pages(&self, cbz: &Path) -> i64 {
        let Some(sidecar) = bunko_library::sidecar::primary_sidecar(cbz) else {
            return 0;
        };
        let series = cbz.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let meta = std::fs::metadata(&sidecar).ok();
        let (entry, _) = bunko_library::compiler::compile_volume(&series, cbz, Some(&sidecar), meta.as_ref());
        bunko_library::schema::missing_page_count(entry.page_count, entry.matched_page_count)
    }
}

/// Facts from the metadata cache (`bunko_library`'s compiler over the database), the
/// production [`LibraryFacts`]. `installed` is the metadata recompile hook (a series'
/// `series.json` after a sidecar landed); `thumbnails` the library index's count of
/// volumes still waiting for a cover.
pub struct StoreFacts {
    pub store: Arc<dyn bunko_library::MetadataStore>,
    pub library: PathBuf,
    pub installed: Option<InstalledHook>,
    pub thumbnails: Option<ThumbnailCount>,
}

/// Called with the archive after a sidecar was installed beside it.
pub type InstalledHook = Arc<dyn Fn(&Path) + Send + Sync>;
/// Volumes waiting for a cover thumbnail.
pub type ThumbnailCount = Arc<dyn Fn() -> i64 + Send + Sync>;

impl LibraryFacts for StoreFacts {
    fn missing_pages(&self, cbz: &Path) -> i64 {
        bunko_library::compiler::missing_pages_now(self.store.as_ref(), &self.library, cbz).unwrap_or(0)
    }

    fn page_count(&self, cbz: &Path) -> Option<i64> {
        bunko_library::compiler::cached_page_count(self.store.as_ref(), &self.library, cbz).ok().flatten()
    }

    fn sidecar_installed(&self, cbz: &Path) {
        if let Some(f) = &self.installed {
            f(cbz);
        }
    }

    fn pending_thumbnails(&self) -> i64 {
        self.thumbnails.as_ref().map_or(0, |f| f())
    }
}

/// The write lock WebDAV writes hold (spec http-webdav §8.7): installing a sidecar takes
/// it for the destination so it cannot interleave with an upload or a folder move.
pub type PathLocks = Arc<dyn bunko_library::service::PathWriteLocks>;

/// [`bunko_dav::PathWriteLocks`] as the lock the collector takes.
pub struct DavLocks(pub bunko_dav::PathWriteLocks);

impl bunko_library::service::PathWriteLocks for DavLocks {
    fn try_lock(&self, path: &Path) -> Option<Box<dyn Send>> {
        self.0.try_lock(path).map(|g| Box::new(g) as Box<dyn Send>)
    }
}

/// Random lowercase hex of `bytes` bytes (`secrets.token_hex`).
pub fn token_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_parts() {
        let j = Job::new("Series A/Vol 1.cbz", "g-1");
        assert_eq!(j.series(), "Series A");
        assert_eq!(j.volume(), "Vol 1");
        let r = Job::new("Loose.cbz", "g-1");
        assert_eq!(r.series(), ".");
        assert_eq!(r.volume(), "Loose");
        assert_eq!(stem_of("A/x.y.cbz"), "x.y");
    }
}
