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
    /// and `MOKURO_NAR_VOCAB`. Run it alone (one pack per process):
    /// `cargo test -p bunko-engines --features nc-models --release --lib -- --ignored --exact torch::textbox::tests::nar_torch_parity`.
    #[test]
    #[ignore = "needs a backend pack with bt_read_chars, a nar-small package, its weights and crops (env vars)"]
    fn nar_torch_parity() {
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
            .textbox(
                crate::models::NAR,
                Precision::Fp32,
                "gpu:0",
                &files,
                8,
                None,
            )
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
        let (mut same, mut differ) = (0usize, Vec::new());
        let t0 = std::time::Instant::now();
        for chunk in rows.chunks(32) {
            let images: Vec<Rgb> = chunk.iter().map(|(f, _)| decode(f)).collect();
            let refs: Vec<&Rgb> = images.iter().collect();
            let got = nar.read_chars(&refs).unwrap();
            assert_eq!(got.len(), chunk.len(), "one result per crop");
            for ((file, want), read) in chunk.iter().zip(&got) {
                if read.text == *want {
                    same += 1;
                } else {
                    differ.push(format!("{file}: {:?} != {want:?}", read.text));
                }
            }
        }
        eprintln!(
            "nar-small on gpu:0 (batch {}): {same} of {} crops identical in {:.1}s",
            nar.info.batch,
            rows.len(),
            t0.elapsed().as_secs_f64()
        );
        assert!(
            differ.is_empty(),
            "{same} of {} identical; {} differ: {differ:#?}",
            rows.len(),
            differ.len()
        );
    }

    /// charspot-small's GPU route against the authors' ONNX model on the CPU
    /// (TEXTBOX-GPU.md §20.3): every crop of `MOKURO_CHARSPOT_PARITY_CROPS` read on `gpu:0`
    /// through the pack's `bt_read_chars`, 64 crops a call, must have its row's `text` in
    /// `MOKURO_CHARSPOT_PARITY_EXPECTED` (`tools/textbox_export/charspot_reference.py`'s
    /// `{"file","text","chars"}` lines), the same characters in the same order, and every
    /// box coordinate within 0.5 px (`verify.same_chars`). A crop the pack leaves to the
    /// CPU route (`chars: None`, over 1,024 characters) fails here: no gate crop has that
    /// many. The variables are this test's own: `MOKURO_TORCH_PACK` (a pack with
    /// `bt_read_chars`), `MOKURO_CHARSPOT_TORCH_PACKAGE` (the dir holding `dense.pt2` and
    /// `tail.pt2`), `MOKURO_CHARSPOT_WEIGHTS` (the authors' `model.safetensors`) and
    /// `MOKURO_CHARSPOT_VOCAB` (their `vocab.json`). Run it alone (one pack per process):
    /// `cargo test -p bunko-engines --features nc-models --release --lib -- --ignored --exact torch::textbox::tests::charspot_torch_parity --nocapture`.
    #[test]
    #[ignore = "needs a backend pack with bt_read_chars, a charspot-small package, its weights and crops (env vars)"]
    fn charspot_torch_parity() {
        const TOLERANCE_PX: f64 = 0.5;
        let var = |k: &str| {
            PathBuf::from(std::env::var_os(k).unwrap_or_else(|| panic!("{k} is not set")))
        };
        let pack = var(PACK_ENV);
        let cache = tempfile::tempdir().unwrap();
        let files = TorchFiles {
            package: var("MOKURO_CHARSPOT_TORCH_PACKAGE"),
            tokenizer: var("MOKURO_CHARSPOT_VOCAB"),
            pos_table: None,
            embeddings: None,
            unpack_cache: Some(cache.path().to_path_buf()),
            weights: vec![var("MOKURO_CHARSPOT_WEIGHTS")],
        };
        let crops = var("MOKURO_CHARSPOT_PARITY_CROPS");
        let expected = std::fs::read_to_string(var("MOKURO_CHARSPOT_PARITY_EXPECTED")).unwrap();
        let rows: Vec<serde_json::Value> = expected
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!rows.is_empty(), "no expected rows");
        let file = |r: &serde_json::Value| {
            r["file"]
                .as_str()
                .unwrap_or_else(|| panic!("a row without file: {r}"))
                .to_string()
        };

        let backend = backend_in(&[], false).unwrap_or_else(|e| panic!("{}: {e}", pack.display()));
        assert!(
            backend.device("gpu:0").is_some(),
            "the pack in {} reports no gpu:0 (opened CPU-only by an earlier test in this process?)",
            pack.display()
        );
        let charspot = backend
            .textbox(
                crate::models::CHARSPOT,
                Precision::Fp32,
                "gpu:0",
                &files,
                8,
                None,
            )
            .unwrap();
        assert!(
            charspot.info.batch >= 1 && charspot.info.weights > 0,
            "{:?}",
            charspot.info
        );

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
        let (mut same, mut differ, mut worst) = (0usize, Vec::new(), 0f64);
        let t0 = std::time::Instant::now();
        for chunk in rows.chunks(64) {
            let images: Vec<Rgb> = chunk.iter().map(|r| decode(&file(r))).collect();
            let refs: Vec<&Rgb> = images.iter().collect();
            let got = charspot.read_chars(&refs).unwrap();
            assert_eq!(got.len(), chunk.len(), "one result per crop");
            for (want, read) in chunk.iter().zip(&got) {
                let name = file(want);
                let wtext = want["text"].as_str().unwrap_or_default();
                let wc = want["chars"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{name}: a row without chars"));
                let Some(chars) = read.chars.as_ref() else {
                    differ.push(format!("{name}: left to the CPU route (chars: None)"));
                    continue;
                };
                let joined: String = chars.iter().map(|c| c.0.as_str()).collect();
                assert_eq!(read.text, joined, "{name}: text is the characters joined");
                let mut why = None;
                if read.text != wtext || chars.len() != wc.len() {
                    why = Some(format!(
                        "{:?} ({} chars) != {wtext:?} ({} chars)",
                        read.text,
                        chars.len(),
                        wc.len()
                    ));
                }
                for (k, (c, w)) in chars.iter().zip(wc).enumerate() {
                    if why.is_some() {
                        break;
                    }
                    if Some(c.0.as_str()) != w[0].as_str() {
                        why = Some(format!("character {k}: {:?} != {}", c.0, w[0]));
                        break;
                    }
                    for (g, i) in [c.1, c.2, c.3, c.4].into_iter().zip(1..5) {
                        let d = (f64::from(g) - w[i].as_f64().unwrap_or(f64::NAN)).abs();
                        worst = worst.max(d);
                        // a NaN on either side fails, as verify.same_chars does
                        if d.is_nan() || d > TOLERANCE_PX {
                            why = Some(format!(
                                "character {k} {:?}: box ({}, {}, {}, {}) != ({}, {}, {}, {})",
                                c.0, c.1, c.2, c.3, c.4, w[1], w[2], w[3], w[4]
                            ));
                            break;
                        }
                    }
                }
                match why {
                    None => same += 1,
                    Some(why) => differ.push(format!("{name}: {why}")),
                }
            }
        }
        eprintln!(
            "charspot-small on gpu:0 (batch {}): {same} of {} crops the same, largest box move {worst:.2e} px, {:.1}s",
            charspot.info.batch,
            rows.len(),
            t0.elapsed().as_secs_f64()
        );
        assert!(
            differ.is_empty(),
            "{same} of {} the same; {} differ: {differ:#?}",
            rows.len(),
            differ.len()
        );
    }
}
