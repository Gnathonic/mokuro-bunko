//! A cloud library on the in-memory `FakeBackend` (feature `testing`).
//!
//! ```ignore
//! let fake = Arc::new(FakeBackend::drive_like());
//! fake.reader_put("mokuro-reader/Series/Vol 1.cbz", &bytes);
//! bunko_server::storage::testing::register(&base, fake.clone());
//! // config: storage.base_path = base, storage.library.backend = any cloud backend
//! let services = Services::new(config, None, &opts)?;
//! fake.reader_rename(..); // the test keeps its handle, across restarts too
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_remote::fake::FakeBackend;
use parking_lot::Mutex;

/// Adjusts the store's settings before it opens (a test's lease or retry timing).
pub type Tune = fn(&mut bunko_remote::RemoteStoreConfig);

type Entry = (Arc<FakeBackend>, Option<Tune>);

static FAKES: Mutex<Option<HashMap<PathBuf, Entry>>> = Mutex::new(None);

/// The next cloud library opened over the storage base path `base` uses `fake`
/// (register it again before each reopen: a restart).
pub fn register(base: &Path, fake: Arc<FakeBackend>) {
    register_tuned(base, fake, None);
}

/// [`register`], with `tune` applied to the store's settings.
pub fn register_tuned(base: &Path, fake: Arc<FakeBackend>, tune: Option<Tune>) {
    FAKES
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(base.to_path_buf(), (fake, tune));
}

pub(super) fn take(base: &Path) -> Option<Entry> {
    FAKES.lock().as_mut()?.remove(base)
}
