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
//! paddle-manga reads its token embeddings from the fp16 table at both precisions:
//! that is what bunko-vlm keeps in memory (f16) and what its parity was measured on;
//! the fp32 table is published but never fetched.

use bunko_ocr::models::{ModelStore, PPOCR_DETECTOR, PPOCR_DICTIONARY, PPOCR_RECOGNIZER, Resolved};
use bunko_vlm::{HayaiAssets, PaddleAssets, Precision};

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

/// Manifest ids of an engine's own files at one precision (PP-OCR not included).
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

/// Every file a machine should fetch ahead of time for `engine` (or every engine):
/// PP-OCR, the engine's fp32 set and, with a GPU, its fp16 set too.
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
            for id in engine_ids(e, p) {
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

/// The engines `describe` offers (see the module docs).
pub fn available_engines(store: &ModelStore) -> Vec<String> {
    let ppocr = obtainable(store, &ppocr_ids());
    if !ppocr {
        return Vec::new();
    }
    ENGINES
        .iter()
        .filter(|e| **e == PPOCR || obtainable(store, &engine_ids(e, Precision::Fp32)))
        .map(|e| e.to_string())
        .collect()
}

fn ensure_all(store: &ModelStore, ids: &[&str]) -> Result<Vec<Resolved>, String> {
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
        assert_eq!(download_plan(Some(HAYAI), false).len(), 8);
        assert_eq!(download_plan(Some(PADDLE), true).len(), 3 + 6 + 4);
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
        assert!(available_engines(&store).is_empty());
        let online = ModelStore::new(
            StoreOptions {
                root: tmp.path().join("models"),
                override_dir: None,
                download: true,
            },
            Manifest::builtin(),
        );
        assert_eq!(
            available_engines(&online),
            vec!["hayai-nova", "paddle-manga", "ppocr-manga"]
        );
    }
}
