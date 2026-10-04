//! A libtorch recognizer behind bunko-vlm's [`Recognizer`] trait: crops are cut here
//! with bunko-vlm's crop code (bit-identical across backends), read by the pack.

use std::sync::Arc;

use bunko_vlm::crop::{LINE_MARGIN_EM, Quad, SECOND_MARGIN_EM, hayai_line_crops, paddle_quad_crop};
use bunko_vlm::{Bgr, CropSet, Recognizer, RecognizerInfo, Rgb, VlmError, read_flat};

use super::loader::{Loaded, Pack};

pub struct TorchRecognizer {
    handle: Loaded,
    info: RecognizerInfo,
    paddle: bool,
    // The handle's functions live in the pack's library: keep it referenced.
    _pack: Arc<Pack>,
}

impl TorchRecognizer {
    pub(crate) fn new(pack: Arc<Pack>, handle: Loaded, info: RecognizerInfo) -> Self {
        Self {
            paddle: info.engine == crate::models::PADDLE,
            handle,
            info,
            _pack: pack,
        }
    }

    /// One text per crop.
    pub fn read_crops(
        &self,
        crops: &[&Rgb],
        caps: Option<&[u32]>,
    ) -> Result<Vec<String>, VlmError> {
        let raw: Vec<(&[u8], u32, u32)> = crops
            .iter()
            .map(|c| (c.data.as_slice(), c.width as u32, c.height as u32))
            .collect();
        self.handle.read(&raw, caps).map_err(VlmError::Runtime)
    }
}

impl Recognizer for TorchRecognizer {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> CropSet {
        if self.paddle {
            CropSet::one(paddle_quad_crop(page, quad, LINE_MARGIN_EM))
        } else {
            CropSet {
                crops: hayai_line_crops(page, quad, vertical),
            }
        }
    }

    fn second_crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> Option<CropSet> {
        self.paddle
            .then(|| CropSet::one(paddle_quad_crop(page, quad, SECOND_MARGIN_EM)))
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        let paddle = self.paddle;
        read_flat(lines, caps, self.info.default_max_tokens, |crops, caps| {
            self.read_crops(crops, paddle.then_some(caps))
        })
    }
}
