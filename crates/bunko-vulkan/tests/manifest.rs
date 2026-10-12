//! The Vulkan model release (bunko-ocr `vulkan_models.json`) lists exactly the files each
//! runtime's adapter loads, for every engine it reads, plus each engine's goldens: a
//! graph format change in a runtime cannot ship with a stale release description.

use bunko_ocr::models::{Manifest, vulkan_golden_id, vulkan_rel};
use bunko_vulkan::backend::{ALL, model_files};

#[test]
fn the_release_lists_every_file_the_runtimes_load() {
    let m = Manifest::builtin();
    for engine in ["hayai-nova", "paddle-manga"] {
        assert!(
            m.get(&vulkan_golden_id(engine)).is_some(),
            "{engine}: no goldens"
        );
        for runtime in ALL {
            let want = model_files(runtime, engine).expect("every runtime reads both engines");
            let prefix = format!("{}/", vulkan_rel(engine, runtime));
            let mut listed: Vec<String> = m
                .files
                .iter()
                .filter_map(|f| f.id.strip_prefix(&prefix).map(str::to_string))
                .collect();
            listed.sort();
            let mut want_sorted = want.clone();
            want_sorted.sort();
            assert_eq!(listed, want_sorted, "{engine} on {runtime}");
            for f in &want {
                let id = format!("{prefix}{f}");
                let e = m.get(&id).unwrap();
                assert_eq!(e.path, id, "store path = id ({id})");
                assert_eq!(e.sha256.len(), 64);
            }
        }
    }
}
