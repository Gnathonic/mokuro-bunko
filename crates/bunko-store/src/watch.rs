//! The local filesystem watcher behind [`crate::LocalStore::watch`].

use std::io;
use std::path::Path;

use crate::store::{WatchGuard, WatchSink};

/// Turns one backend event into the relevant [`crate::WatchEvent`]s.
#[cfg(feature = "watch")]
pub type Translate = std::sync::Arc<dyn Fn(&notify::Event) -> Vec<crate::WatchEvent> + Send + Sync>;

/// Without the `watch` feature there is no backend event to translate.
#[cfg(not(feature = "watch"))]
pub type Translate = std::sync::Arc<dyn Fn(&()) -> Vec<crate::WatchEvent> + Send + Sync>;

/// Watch `root` (created first) recursively; every translated event goes to `sink`
/// on the backend's thread. A full kernel queue becomes [`crate::WatchEvent::Rescan`].
#[cfg(feature = "watch")]
pub fn watch_local(
    root: &Path,
    translate: Option<&Translate>,
    sink: WatchSink,
) -> io::Result<WatchGuard> {
    use notify::{RecursiveMode, Watcher};

    std::fs::create_dir_all(root)?;
    let translate = translate.cloned();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if let Some(translate) = &translate {
                    for change in translate(&event) {
                        sink(change);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "[FS-WATCHER] watch error");
                if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
                    sink(crate::WatchEvent::Rescan);
                }
            }
        })
        .map_err(notify_io)?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(notify_io)?;
    tracing::info!("[FS-WATCHER] Watching {}", root.display());
    Ok(WatchGuard(Box::new(watcher)))
}

#[cfg(feature = "watch")]
fn notify_io(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(io) => io,
        _ => io::Error::other(e),
    }
}

#[cfg(not(feature = "watch"))]
pub fn watch_local(
    _root: &Path,
    _translate: Option<&Translate>,
    _sink: WatchSink,
) -> io::Result<WatchGuard> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "built without the watch feature",
    ))
}
