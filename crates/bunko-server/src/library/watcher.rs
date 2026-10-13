//! The library filesystem watcher (0.5.2 `middleware/fs_watcher.py`, spec http-webdav
//! §12.1): `notify` recursive on the library root, created/deleted/moved only, each
//! relevant side of a move reported on its own, relevance per
//! [`bunko_library::paths::is_relevant_change`] (directories, `.cbz .mokuro .gz .webp`,
//! never `.json`, so the compiler's own writes cannot re-trigger it). No debounce here:
//! the metadata timers debounce.

use std::path::Path;
use std::sync::Arc;

use bunko_library::paths::is_relevant_change;
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind};

pub use bunko_store::WatchEvent;

/// Watching runs while this value lives; dropping it stops the backend thread.
pub struct LibraryWatcher {
    _guard: bunko_store::WatchGuard,
}

/// Whether a path that no longer exists was a directory. The backend cannot say for
/// the source side of a rename, so: a name with no `.ext` suffix is taken for a folder
/// (series folders), anything with one for a file (the compiler's and the uploader's
/// staging files all end `.tmp`, which must not read as a folder move).
pub(super) fn vanished_is_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| bunko_library::sidecar::py_suffix(name).is_empty())
}

fn stat_is_dir(path: &Path) -> Option<bool> {
    bunko_store::fs::metadata(path).ok().map(|m| m.is_dir())
}

/// The relevant [`WatchEvent`]s of one backend event.
pub fn translate(event: &Event) -> Vec<WatchEvent> {
    if event.need_rescan() {
        return vec![WatchEvent::Rescan];
    }
    let mut out = Vec::new();
    for path in &event.paths {
        let change = match event.kind {
            EventKind::Create(kind) => {
                let is_dir = match kind {
                    CreateKind::Folder => true,
                    CreateKind::File => false,
                    _ => stat_is_dir(path).unwrap_or(false),
                };
                WatchEvent::Added {
                    path: path.clone(),
                    is_dir,
                }
            }
            EventKind::Remove(kind) => {
                let is_dir = match kind {
                    RemoveKind::Folder => true,
                    RemoveKind::File => false,
                    _ => vanished_is_dir(path),
                };
                WatchEvent::Removed {
                    path: path.clone(),
                    is_dir,
                }
            }
            // inotify reports a rename as From + To + Both; Both repeats the first two.
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => continue,
            EventKind::Modify(ModifyKind::Name(mode)) => match (mode, stat_is_dir(path)) {
                (RenameMode::From, _) | (_, None) => WatchEvent::Removed {
                    path: path.clone(),
                    is_dir: vanished_is_dir(path),
                },
                (_, Some(is_dir)) => WatchEvent::Added {
                    path: path.clone(),
                    is_dir,
                },
            },
            _ => continue,
        };
        let (WatchEvent::Added { path, is_dir } | WatchEvent::Removed { path, is_dir }) = &change
        else {
            continue;
        };
        if is_relevant_change(path, *is_dir) {
            out.push(change);
        }
    }
    out
}

impl LibraryWatcher {
    /// Watch `root` (created first, as 0.5.2 did) recursively. `on_event` runs on the
    /// backend's thread for every relevant change. A remote store mounted at `root`
    /// delivers its own change feed; the local disk is watched through
    /// [`bunko_store::LocalStore::watch`] with [`translate`].
    pub fn start(
        root: &Path,
        on_event: impl Fn(WatchEvent) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        use bunko_store::LibraryStore;
        let sink: bunko_store::WatchSink = Arc::new(on_event);
        let guard = match bunko_store::store_for(root) {
            Some(store) if store.caps().remote => store.watch(sink)?,
            _ => {
                let translate: bunko_store::Translate = Arc::new(translate);
                bunko_store::LocalStore::with_translator(root, translate).watch(sink)?
            }
        };
        Ok(Self { _guard: guard })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ev(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn relevance_and_kinds() {
        let created = translate(&ev(EventKind::Create(CreateKind::File), "/lib/A/v.cbz"));
        assert_eq!(
            created,
            vec![WatchEvent::Added {
                path: "/lib/A/v.cbz".into(),
                is_dir: false
            }]
        );
        assert!(
            translate(&ev(
                EventKind::Create(CreateKind::File),
                "/lib/A/series.json"
            ))
            .is_empty()
        );
        assert!(translate(&ev(EventKind::Create(CreateKind::File), "/lib/A/.x.tmp")).is_empty());
        assert_eq!(
            translate(&ev(EventKind::Remove(RemoveKind::Folder), "/lib/A")).len(),
            1
        );
        // The compiler's staging file renamed onto series.json: neither side is relevant.
        assert!(
            translate(&ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                "/lib/A/.series.json.compile-x.tmp"
            ))
            .is_empty()
        );
        // A folder moved away: reads as a directory.
        assert_eq!(
            translate(&ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                "/nonexistent-lib/Series B"
            )),
            vec![WatchEvent::Removed {
                path: "/nonexistent-lib/Series B".into(),
                is_dir: true
            }]
        );
        assert!(
            translate(&ev(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                "/lib/A/v.cbz"
            ))
            .is_empty()
        );
    }
}
