//! Which model files each engine needs, and fetching them through `bunko-ocr`'s
//! [`ModelStore`] (the builtin manifest: PP-OCR from its pinned Hugging Face revision,
//! hayai-nova and paddle-manga from the `models-v1` release).
//!
//! Availability ("installing" semantics, reported by `describe`): an engine is offered
//! when every file of its fp32 set is on disk **or** this machine may download it.
//! With downloads on (the default) every engine is offered from the start and the
//! first session that opens it downloads what is missing (progress in the log). With
//! downloads off (`MOKURO_MODELS_DOWNLOAD=0`) an engine appears only once its files
//! are in place; a catalog with no engines means "still installing" and matches no
//! row.
//!
//! The recognizers run on libtorch (feature `torch`): a compiled package per engine x
//! precision x device target (`bunko_ocr::models::ModelStore::ensure_torch_package`)
//! plus the host files listed by [`torch_host_ids`] (tokenizers, hayai-nova's position
//! table and token embeddings; paddle-manga's input embeddings come with its packages'
//! weights, [`paddle_embed_id`] for a package without them). paddle-manga runs on a GPU
//! only (`bunko_sched::precision::gpu_only`): the release has no CPU packages for it.
//! The ONNX graphs ([`engine_ids`]) are only used by the deferred ONNX recognizers
//! (feature `onnx-vlm`).
//!
//! paddle-manga's input embeddings: the ONNX recognizer reads the fp16 table at both
//! precisions (what bunko-vlm keeps in memory and measured parity on). The libtorch one
//! casts the fp32 table to the package precision (bit-identical to 0.5.2's model
//! weights at fp32/bf16/fp16; the fp16 table is enough for fp16).

use bunko_ocr::models::{ModelStore, PPOCR_DETECTOR, PPOCR_DICTIONARY, PPOCR_RECOGNIZER, Resolved};
use bunko_vlm::Precision;
#[cfg(feature = "onnx-vlm")]
use bunko_vlm::{HayaiAssets, PaddleAssets};

pub const PPOCR: &str = "ppocr-manga";
pub const HAYAI: &str = "hayai-nova";
pub const PADDLE: &str = "paddle-manga";
/// Every engine this build runs, in catalog order.
pub const ENGINES: [&str; 3] = [HAYAI, PADDLE, PPOCR];

/// The `ocr_engine.weights` entry naming the ONNX export set: `{repo: release tag}`.
pub const EXPORT_REPO: &str = "Gnathonic/mokuro-bunko";

/// Manifest ids of the PP-OCR files (every engine reads lines with them).
pub fn ppocr_ids() -> Vec<&'static str> {
    vec![PPOCR_DETECTOR, PPOCR_RECOGNIZER, PPOCR_DICTIONARY]
}

/// Host files the libtorch recognizer of `engine` always reads besides its compiled
/// package. paddle-manga's input embeddings come with its (GPU) packages' weights; only
/// a package without them (an older or hand-made one) also needs [`paddle_embed_id`].
pub fn torch_host_ids(engine: &str, _precision: Precision) -> Vec<&'static str> {
    match engine {
        HAYAI => vec![
            "hayai-nova/pos-table",
            "hayai-nova/token-embeddings",
            "hayai-nova/tokenizer",
        ],
        PADDLE => vec!["paddle-manga/tokenizer"],
        _ => Vec::new(),
    }
}

/// paddle-manga's input-embedding table for packages that do not carry it: the fp32
/// table (cast to bf16 on load) or, for fp16, the fp16 one (both bit-identical to the
/// model's own table at that precision).
pub fn paddle_embed_id(precision: Precision) -> &'static str {
    if precision == Precision::Fp16 {
        "paddle-manga/embed-fp16"
    } else {
        "paddle-manga/embed-fp32"
    }
}

/// The host files of a libtorch recognizer, fetched if needed. `package` is the
/// package directory (`<engine>/<precision>/<target>`); shared weights sit in its
/// parent.
#[cfg(feature = "torch")]
pub fn torch_host_files(
    store: &ModelStore,
    engine: &str,
    precision: Precision,
    package: std::path::PathBuf,
) -> Result<crate::torch::TorchFiles, String> {
    let r = ensure_all(store, &torch_host_ids(engine, precision))?;
    Ok(match engine {
        HAYAI => crate::torch::TorchFiles {
            package,
            pos_table: Some(r[0].path.clone()),
            embeddings: Some(r[1].path.clone()),
            tokenizer: r[2].path.clone(),
            unpack_cache: None,
        },
        _ => {
            let blob = package
                .parent()
                .is_some_and(|p| p.join("weights-decoder.safetensors").is_file());
            let embeddings = if blob {
                None
            } else {
                Some(
                    ensure_all(store, &[paddle_embed_id(precision)])?[0]
                        .path
                        .clone(),
                )
            };
            crate::torch::TorchFiles {
                package,
                pos_table: None,
                embeddings,
                tokenizer: r[0].path.clone(),
                unpack_cache: None,
            }
        }
    })
}

/// Manifest ids of an engine's ONNX files at one precision (PP-OCR not included).
pub fn engine_ids(engine: &str, precision: Precision) -> Vec<&'static str> {
    let fp16 = precision == Precision::Fp16;
    match engine {
        HAYAI => vec![
            if fp16 {
                "hayai-nova/vision-fp16"
            } else {
                "hayai-nova/vision-fp32"
            },
            if fp16 {
                "hayai-nova/decoder-fp16"
            } else {
                "hayai-nova/decoder-fp32"
            },
            "hayai-nova/pos-table",
            "hayai-nova/token-embeddings",
            "hayai-nova/tokenizer",
        ],
        PADDLE => {
            if fp16 {
                vec![
                    "paddle-manga/vision-fp16",
                    "paddle-manga/vision-data-fp16",
                    "paddle-manga/decoder-fp16",
                    "paddle-manga/decoder-data-fp16",
                    "paddle-manga/embed-fp16",
                    "paddle-manga/tokenizer",
                ]
            } else {
                vec![
                    "paddle-manga/vision-fp32",
                    "paddle-manga/vision-data-fp32",
                    "paddle-manga/decoder-fp32",
                    "paddle-manga/decoder-data-fp32",
                    "paddle-manga/embed-fp16",
                    "paddle-manga/tokenizer",
                ]
            }
        }
        _ => Vec::new(),
    }
}

/// Manifest ids a recognizer reads at `precision` with the backends of this build:
/// the libtorch host files, and the ONNX graphs when the ONNX recognizers are built.
pub fn recognizer_ids(engine: &str, precision: Precision) -> Vec<&'static str> {
    let mut ids = Vec::new();
    if cfg!(feature = "torch") {
        ids.extend(torch_host_ids(engine, precision));
    }
    if cfg!(feature = "onnx-vlm") {
        ids.extend(engine_ids(engine, precision));
    }
    ids
}

/// The manifest files `models list` shows for `engine` in this build: PP-OCR, the
/// recognizer's host files (and the ONNX graphs only when the ONNX recognizers are
/// built). The compiled libtorch packages depend on the device and are reported per
/// device (`EnginePipeline::package_status`).
pub fn list_ids(engine: &str) -> Vec<&'static str> {
    if engine == PPOCR {
        return ppocr_ids();
    }
    let mut ids: Vec<&'static str> = Vec::new();
    for p in [Precision::Fp32, Precision::Fp16] {
        for id in recognizer_ids(engine, p) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Every device-independent file a machine should fetch ahead of time for `engine`
/// (or every engine): PP-OCR, the engine's fp32 set and, with a GPU, its fp16 set
/// too. The compiled libtorch packages depend on the device: see
/// `EnginePipeline::prefetch`.
pub fn download_plan(engine: Option<&str>, gpu: bool) -> Vec<&'static str> {
    let mut ids = ppocr_ids();
    for e in ENGINES {
        if engine.is_some_and(|want| want != e) {
            continue;
        }
        let mut precisions = vec![Precision::Fp32];
        if gpu {
            precisions.push(Precision::Fp16);
        }
        for p in precisions {
            for id in recognizer_ids(e, p) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Every file is on disk, or may be downloaded.
pub fn obtainable(store: &ModelStore, ids: &[&str]) -> bool {
    store.can_download() || ids.iter().all(|id| store.locate(id).is_some())
}

/// The engines `describe` offers (see the module docs). `recognizer_ready(engine)`
/// says whether a recognizer backend can run the engine here (a libtorch package or
/// the ONNX graphs are obtainable).
pub fn available_engines(
    store: &ModelStore,
    recognizer_ready: impl Fn(&str) -> bool,
) -> Vec<String> {
    let ppocr = obtainable(store, &ppocr_ids());
    if !ppocr {
        return Vec::new();
    }
    ENGINES
        .iter()
        .filter(|e| **e == PPOCR || recognizer_ready(e))
        .map(|e| e.to_string())
        .collect()
}

pub(crate) fn ensure_all(store: &ModelStore, ids: &[&str]) -> Result<Vec<Resolved>, String> {
    let missing: Vec<&&str> = ids.iter().filter(|id| store.locate(id).is_none()).collect();
    if !missing.is_empty() {
        let bytes: u64 = missing
            .iter()
            .filter_map(|id| store.manifest().get(id))
            .map(|f| f.size)
            .sum();
        tracing::info!(
            "fetching {} model file(s), {} MB, into {}",
            missing.len(),
            bytes / 1_000_000,
            store.options().root.display()
        );
    }
    ids.iter()
        .map(|id| store.ensure(id).map_err(|e| e.to_string()))
        .collect()
}

/// The hayai-nova assets at `precision`, fetched if needed.
#[cfg(feature = "onnx-vlm")]
pub fn hayai_assets(store: &ModelStore, precision: Precision) -> Result<HayaiAssets, String> {
    let r = ensure_all(store, &engine_ids(HAYAI, precision))?;
    Ok(HayaiAssets {
        vision: r[0].path.clone(),
        decoder: r[1].path.clone(),
        pos_table: r[2].path.clone(),
        token_embeddings: r[3].path.clone(),
        tokenizer: r[4].path.clone(),
    })
}

/// The paddle-manga assets at `precision`, fetched if needed. The external-data file
/// must sit beside its graph under the name the graph records; a store or release
/// directory has that layout.
#[cfg(feature = "onnx-vlm")]
pub fn paddle_assets(store: &ModelStore, precision: Precision) -> Result<PaddleAssets, String> {
    let r = ensure_all(store, &engine_ids(PADDLE, precision))?;
    for (graph, data) in [(&r[0], &r[1]), (&r[2], &r[3])] {
        let expected = graph.path.with_file_name(format!(
            "{}.data",
            graph
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        if data.path != expected {
            return Err(format!(
                "{} must be next to {} (found it at {})",
                expected.display(),
                graph.path.display(),
                data.path.display()
            ));
        }
    }
    Ok(PaddleAssets {
        vision: r[0].path.clone(),
        decoder: r[2].path.clone(),
        embed: r[4].path.clone(),
        tokenizer: r[5].path.clone(),
    })
}

/// The PP-OCR files, fetched if needed.
pub fn ppocr_files(store: &ModelStore) -> Result<bunko_ocr::models::PpocrFiles, String> {
    ensure_all(store, &ppocr_ids())?;
    store.ppocr().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunko_ocr::models::{Manifest, StoreOptions};

    #[test]
    fn every_id_is_in_the_manifest() {
        let m = Manifest::builtin();
        for id in download_plan(None, true) {
            assert!(m.get(id).is_some(), "{id}");
        }
        assert_eq!(download_plan(Some(PPOCR), true).len(), 3);
        let (torch, onnx) = (cfg!(feature = "torch"), cfg!(feature = "onnx-vlm"));
        let hayai = 3 + if onnx {
            5
        } else if torch {
            3
        } else {
            0
        };
        assert_eq!(download_plan(Some(HAYAI), false).len(), hayai);
        // paddle: torch the tokenizer; ONNX 6 + 4 more (shares the tokenizer)
        let paddle = 3 + match (torch, onnx) {
            (_, true) => 6 + 4,
            (true, false) => 1,
            (false, false) => 0,
        };
        assert_eq!(download_plan(Some(PADDLE), true).len(), paddle);
        for p in [Precision::Fp32, Precision::Bf16, Precision::Fp16] {
            for e in [HAYAI, PADDLE] {
                for id in torch_host_ids(e, p) {
                    assert!(m.get(id).is_some(), "{id}");
                }
                assert!(m.get(paddle_embed_id(p)).is_some());
            }
        }
    }

    #[test]
    fn offline_and_empty_means_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: false,
            },
            Manifest::builtin(),
        );
        assert!(available_engines(&store, |_| true).is_empty());
        let online = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: true,
            },
            Manifest::builtin(),
        );
        assert_eq!(
            available_engines(&online, |_| true),
            vec!["hayai-nova", "paddle-manga", "ppocr-manga"]
        );
        // no recognizer backend: ppocr-manga only
        assert_eq!(available_engines(&online, |_| false), vec!["ppocr-manga"]);
    }
}
