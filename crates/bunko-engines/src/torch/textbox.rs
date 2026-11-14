//! A text-box recognizer (nar-small) on the pack: whole text-box crops in, one
//! [`CharsRead`] per crop out, through the pack's optional `bt_read_chars`. The
//! pack cuts nothing and decodes the ids itself; an empty crop reads `""` in its place.

use std::sync::Arc;

use bunko_torch::abi::{CharsRead, LoadInfo};
use bunko_vlm::{Rgb, VlmError};

use super::loader::{Loaded, Pack};

pub struct TorchTextbox {
    /// What the pack reported at load (`bt_info`).
    pub info: LoadInfo,
    handle: Loaded,
    // The handle's functions live in the pack's library: keep it referenced.
    _pack: Arc<Pack>,
}

impl TorchTextbox {
    pub(crate) fn new(pack: Arc<Pack>, handle: Loaded, info: LoadInfo) -> Self {
        Self {
            info,
            handle,
            _pack: pack,
        }
    }

    /// Each crop's text (and its characters' boxes, for an engine that has them), in
    /// crop order. The pack splits the crops into batches of its own.
    pub fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError> {
        let raw: Vec<(&[u8], u32, u32)> = crops
            .iter()
            .map(|c| (c.data.as_slice(), c.width as u32, c.height as u32))
            .collect();
        self.handle.read_chars(&raw).map_err(VlmError::Runtime)
    }
}
