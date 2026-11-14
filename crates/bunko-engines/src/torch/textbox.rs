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
        let raw = super::raw_crops(crops).map_err(VlmError::Runtime)?;
        self.handle.read_chars(&raw).map_err(VlmError::Runtime)
    }
}

// nar-small is a non-commercial model: its test needs the feature.
#[cfg(all(test, feature = "nc-models"))]
mod tests {
    use std::path::PathBuf;

    use bunko_vlm::{Precision, Rgb};

    use crate::torch::{PACK_ENV, TorchFiles, backend_in};

    /// The GPU route against the authors' fp32 PyTorch: every crop of
    /// `MOKURO_NAR_PARITY_CROPS` read on `gpu:0` through the pack's `bt_read_chars`, 32
    /// crops a call, must give the `text` of its row in `MOKURO_NAR_PARITY_EXPECTED`
    /// (`{"file","ids","text"}` rows from `tools/textbox_export/check_parity.py
    /// --expected`). fp32 has no tolerance. The variables are this test's own:
    /// `MOKURO_TORCH_PACK` (a pack with `bt_read_chars`), `MOKURO_NAR_TORCH_PACKAGE` (the
    /// dir holding `model.pt2`), `MOKURO_NAR_WEIGHTS` (the authors' `model.safetensors`)
    /// and `MOKURO_NAR_VOCAB`; `MOKURO_NAR_PARITY_READS`, when set, is a file the test
    /// writes every crop's text to (`{"file","text"}` lines, for `python -m torch_export
    /// compare-reads` and `block-preds`). Run it alone (one pack per process):
    /// `cargo test -p bunko-engines --features nc-models --release --lib -- --ignored --exact torch::textbox::tests::nar_torch_parity`.
    #[test]
    #[ignore = "needs a backend pack with bt_read_chars, a nar-small package, its weights and crops (env vars)"]
    fn nar_torch_parity() {
        nar_parity(Precision::Fp32, |_| 0);
    }

    /// The same at fp16 (TEXTBOX-GPU.md §11, §20.4): `MOKURO_NAR_TORCH_PACKAGE` is an fp16
    /// package (`bunko.weights_dtype = fp16`), bound to the same fp32 `MOKURO_NAR_WEIGHTS`,
    /// which the pack casts at load. At most `floor(0.001 x crops)` texts may differ from
    /// `MOKURO_NAR_PARITY_EXPECTED`, which is the authors' fp16 output
    /// (`check_parity.py --precision fp16`) or their fp32 output (O2: fp16 against fp32);
    /// run it once with each. Run it alone:
    /// `cargo test -p bunko-engines --features nc-models --release --lib -- --ignored --exact torch::textbox::tests::nar_torch_parity_fp16`.
    #[test]
    #[ignore = "needs a backend pack with bt_read_chars, an fp16 nar-small package, its weights and crops (env vars)"]
    fn nar_torch_parity_fp16() {
        nar_parity(Precision::Fp16, |crops| crops / 1000);
    }

    /// Read every expected crop through `bt_read_chars` on `gpu:0` at `precision`; at most
    /// `limit(crops)` texts may differ.
    fn nar_parity(precision: Precision, limit: fn(usize) -> usize) {
        let var = |k: &str| {
            PathBuf::from(std::env::var_os(k).unwrap_or_else(|| panic!("{k} is not set")))
        };
        // The backend reads MOKURO_TORCH_PACK itself; fail early with the variable's name.
        let pack = var(PACK_ENV);
        let cache = tempfile::tempdir().unwrap();
        let files = TorchFiles {
            package: var("MOKURO_NAR_TORCH_PACKAGE"),
            tokenizer: var("MOKURO_NAR_VOCAB"),
            pos_table: None,
            embeddings: None,
            unpack_cache: Some(cache.path().to_path_buf()),
            weights: vec![var("MOKURO_NAR_WEIGHTS")],
        };
        let crops = var("MOKURO_NAR_PARITY_CROPS");
        let expected = std::fs::read_to_string(var("MOKURO_NAR_PARITY_EXPECTED")).unwrap();
        let reads_to = std::env::var_os("MOKURO_NAR_PARITY_READS").map(PathBuf::from);
        let rows: Vec<(String, String)> = expected
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                let field = |k: &str| {
                    row[k]
                        .as_str()
                        .unwrap_or_else(|| panic!("a row without {k}: {line}"))
                        .to_string()
                };
                (field("file"), field("text"))
            })
            .collect();
        assert!(!rows.is_empty(), "no expected rows");

        let backend = backend_in(&[], false).unwrap_or_else(|e| panic!("{}: {e}", pack.display()));
        assert!(
            backend.device("gpu:0").is_some(),
            "the pack in {} reports no gpu:0 (opened CPU-only by an earlier test in this process?)",
            pack.display()
        );
        let nar = backend
            .textbox(crate::models::NAR, precision, "gpu:0", &files, 8, None)
            .unwrap();
        assert!(nar.info.batch >= 1, "{:?}", nar.info);

        let decode = |file: &str| -> Rgb {
            let img = image::open(crops.join(file))
                .unwrap_or_else(|e| panic!("{file}: {e}"))
                .to_rgb8();
            Rgb {
                width: img.width() as usize,
                height: img.height() as usize,
                data: img.into_raw(),
            }
        };
        let (mut same, mut differ, mut reads) = (0usize, Vec::new(), String::new());
        let t0 = std::time::Instant::now();
        for chunk in rows.chunks(32) {
            let images: Vec<Rgb> = chunk.iter().map(|(f, _)| decode(f)).collect();
            let refs: Vec<&Rgb> = images.iter().collect();
            let got = nar.read_chars(&refs).unwrap();
            assert_eq!(got.len(), chunk.len(), "one result per crop");
            for ((file, want), read) in chunk.iter().zip(&got) {
                if reads_to.is_some() {
                    let row = serde_json::json!({"file": file, "text": read.text});
                    reads.push_str(&format!("{row}\n"));
                }
                if read.text == *want {
                    same += 1;
                } else {
                    differ.push(format!("{file}: {:?} != {want:?}", read.text));
                }
            }
        }
        let limit = limit(rows.len());
        eprintln!(
            "nar-small {} on gpu:0 (batch {}): {same} of {} crops identical, {} differ (at most {limit}) in {:.1}s",
            precision.as_str(),
            nar.info.batch,
            rows.len(),
            differ.len(),
            t0.elapsed().as_secs_f64()
        );
        if let Some(path) = reads_to {
            std::fs::write(&path, reads).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        assert!(
            differ.len() <= limit,
            "{same} of {} identical; {} differ, at most {limit} may: {differ:#?}",
            rows.len(),
            differ.len()
        );
    }
}
