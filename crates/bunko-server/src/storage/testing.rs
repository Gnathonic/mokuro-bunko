//! A cloud library on the in-memory `FakeBackend` (feature `testing`).
//!
//! ```ignore
//! let fake = FakeBackend::drive_like();
//! fake.reader_put("mokuro-reader/Series/Vol 1.cbz", &bytes);
//! bunko_server::storage::testing::register(&base, fake);
//! // config: storage.base_path = base, storage.library.backend = any cloud backend
//! let services = Services::new(config, None, &opts)?;
//! let store = services.storage.as_ref().unwrap().fake_store().unwrap();
//! store.backend().reader_rename(..);
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bunko_remote::fake::FakeBackend;
use parking_lot::Mutex;

static FAKES: Mutex<Option<HashMap<PathBuf, FakeBackend>>> = Mutex::new(None);

/// The next cloud library opened over the storage base path `base` uses `fake`.
pub fn register(base: &Path, fake: FakeBackend) {
    FAKES
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(base.to_path_buf(), fake);
}

pub(super) fn take(base: &Path) -> Option<FakeBackend> {
    FAKES.lock().as_mut()?.remove(base)
}
