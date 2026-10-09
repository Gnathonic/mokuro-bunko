//! The engine pipeline through the processor seam (`PagePipeline` / `VolumeRunner`) on
//! real pages with the real models: progress order, blank and omitted pages,
//! `failed_pages`, the sidecar's shape, volume failures, cancellation, overlapping
//! volumes, the stats window, and the refusals that happen before any model loads.
//!
//! Needs the model files and a sample archive, hence `#[ignore]`:
//!
//! ```text
//! MOKURO_MODELS_DIR=~/.cache/mokuro-bunko-demo/models-v1 \
//! CARGO_TARGET_DIR=target/agent-engines cargo test -p bunko-engines --release \
//!     --test pipeline -- --ignored --nocapture
//! ```
//!
//! `BUNKO_SAMPLE_CBZ` names the archive pages are taken from (default: Dr Stone 01 in
//! `~/Downloads`, read only).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_processor::{CancelToken, PagePipeline, PageProgress, RunError, VolumeMeta};
use bunko_proto::{PoolsSpec, RowSpec};

fn sample() -> Option<PathBuf> {
    let p = std::env::var_os("BUNKO_SAMPLE_CBZ")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Downloads/Dr Stone (HD Scan) - Dr Stone 01.cbz")
        });
    p.is_file().then_some(p)
}

/// `n` real pages from `src` (natural order, from the 11th on), as `(name, bytes)`.
fn real_pages(src: &Path, n: usize) -> Vec<(String, Vec<u8>)> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let mut names: Vec<String> = z
        .file_names()
        .filter(|n| n.ends_with(".webp") || n.ends_with(".jpg") || n.ends_with(".png"))
        .map(str::to_string)
        .collect();
    names.sort();
    names
        .into_iter()
        .skip(10)
        .take(n)
        .map(|name| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut z.by_name(&name).unwrap(), &mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

fn write_cbz(path: &Path, members: &[(String, Vec<u8>)]) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, bytes) in members {
        w.start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap();
}

/// A PNG cut half way through its pixel data (an interrupted upload): the size is
/// readable, the pixels are not.
fn truncated_png(w: u32, h: u32) -> Vec<u8> {
    let mut png = Vec::new();
    image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x * 7 + y * 13) as u8, (x ^ y) as u8, (x * y) as u8])
    })
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .unwrap();
    png.truncate(png.len() / 2);
    png
}

fn spec(engine: &str, precision: &str) -> RowSpec {
    RowSpec {
        id: "g-1".into(),
        name: engine.into(),
        engine: engine.into(),
        detector: None,
        patch_budget: 512,
        precision: precision.into(),
        pools: PoolsSpec::default(),
        precision_pick: None,
        precision_why: String::new(),
        primary: true,
    }
}

fn meta(stem: &str) -> VolumeMeta {
    VolumeMeta {
        claim: format!("c-{stem}"),
        title: "Series".into(),
        volume: String::new(),
        title_uuid: Some("title-uuid".into()),
        volume_uuid: None,
        stem: stem.into(),
        sidecar_name: format!("{stem}.mokuro"),
    }
}

fn pipeline(dir: &Path) -> EnginePipeline {
    EnginePipeline::new(EngineConfig::new(dir.join("models"), Backend::Cpu))
}

#[test]
#[ignore = "needs the model files and a sample archive"]
fn ppocr_volume_with_bad_pages() {
    let Some(src) = sample() else {
        eprintln!("no sample archive; skipped");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    let info = p.describe();
    assert!(
        info.catalog.engines.contains(&"ppocr-manga".to_string()),
        "{:?}",
        info.catalog
    );
    assert_eq!(info.catalog.detectors, vec!["ppocr-manga"]);
    assert_eq!(info.catalog.devices[0].id, "cpu");

    let mut members = real_pages(&src, 3);
    let base = members[0].0.rsplit_once('/').map(|(d, _)| d.to_string());
    let name = |f: &str| match &base {
        Some(d) => format!("{d}/{f}"),
        None => f.to_string(),
    };
    members.push((name("zz 900.png"), truncated_png(640, 960)));
    members.push((name("zz 901.jpg"), b"not an image at all".to_vec()));
    members.push(("notes.txt".into(), b"ignored".to_vec()));
    let cbz = tmp.path().join("Vol.cbz");
    write_cbz(&cbz, &members);

    let runner = p.open(&spec("ppocr-manga", "auto-accuracy")).unwrap();
    let ready = runner.ready();
    assert_eq!(ready.stage_workers.len(), 2);
    assert!(
        ready.pipeline.starts_with("detect (cpu x"),
        "{}",
        ready.pipeline
    );
    assert_eq!(ready.precision, None);
    assert_eq!(
        ready
            .weights
            .get("Kellenok/PP-OCRv6_manga")
            .map(String::as_str),
        Some("ba1d479e8a61a20e8318c9758c73fbbbd290b98d")
    );

    let events = Mutex::new(Vec::new());
    let out = tmp.path().join("out/Vol.mokuro");
    let outcome = runner
        .run_volume(
            &cbz,
            &meta("Vol"),
            &out,
            &|e| events.lock().unwrap().push(e),
            &CancelToken::new(),
        )
        .unwrap();
    let events = events.into_inner().unwrap();
    assert_eq!(events[0], PageProgress::Started { pages: 5 });
    let done: Vec<u32> = events[1..]
        .iter()
        .map(|e| match e {
            PageProgress::Page { done, total: 5 } => *done,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(done, vec![1, 2, 3, 4, 5]);
    assert_eq!(
        outcome.pages, 4,
        "3 read + 1 blank; the headerless one is omitted"
    );
    assert_eq!(outcome.failed_pages, 2);
    let stats = outcome.stats.unwrap();
    assert_eq!(stats.items, 5);
    assert_eq!(
        stats
            .queues
            .iter()
            .map(|q| q.name.as_str())
            .collect::<Vec<_>>(),
        ["in->detect", "detect->layout", "layout->out"]
    );
    assert!(runner.stats().unwrap().items >= 5);

    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!out.with_extension("mokuro.tmp").exists());
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "version",
            "title",
            "title_uuid",
            "volume",
            "volume_uuid",
            "ocr_engine",
            "pages"
        ]
    );
    assert_eq!(v["version"], "0.2.5");
    assert_eq!(v["title"], "Series");
    assert_eq!(v["title_uuid"], "title-uuid");
    assert_eq!(
        v["volume"], "Vol",
        "an empty volume title falls back to the stem"
    );
    assert_eq!(v["volume_uuid"].as_str().unwrap().len(), 36);
    assert_eq!(v["ocr_engine"]["id"], "ppocr-manga");
    assert!(v["ocr_engine"].get("precision").is_none());
    let pages = v["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 4);
    for p in &pages[..3] {
        assert!(
            !p["blocks"].as_array().unwrap().is_empty(),
            "{}",
            p["img_path"]
        );
    }
    assert_eq!(pages[3]["img_path"], name("zz 900.png"));
    assert_eq!(pages[3]["img_width"], 640);
    assert_eq!(pages[3]["img_height"], 960);
    assert_eq!(pages[3]["blocks"], serde_json::json!([]));
    assert!(
        text.contains("\", \"") || text.contains(", \""),
        "default separators"
    );

    // Every page failing fails the volume; no pages at all too; nothing is written.
    let bad = tmp.path().join("Bad.cbz");
    write_cbz(&bad, &[("1.jpg".into(), b"junk".to_vec())]);
    let err = runner
        .run_volume(
            &bad,
            &meta("Bad"),
            &tmp.path().join("Bad.mokuro"),
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(err, RunError::Volume("every page failed".into()));
    assert!(!tmp.path().join("Bad.mokuro").exists());
    let empty = tmp.path().join("Empty.cbz");
    write_cbz(&empty, &[("a.txt".into(), b"x".to_vec())]);
    let err = runner
        .run_volume(
            &empty,
            &meta("Empty"),
            &tmp.path().join("E.mokuro"),
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(
        err,
        RunError::Volume("no page images found in Empty.cbz".into())
    );

    // Cancelled before it starts: no sidecar.
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = runner
        .run_volume(
            &cbz,
            &meta("Vol"),
            &tmp.path().join("C.mokuro"),
            &|_| {},
            &cancel,
        )
        .unwrap_err();
    assert_eq!(err, RunError::Cancelled);
    assert!(!tmp.path().join("C.mokuro").exists());

    // Two volumes at once (the runtime's overlap): both finish, in full.
    assert_eq!(runner.overlap(), 2);
    let r = &runner;
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| {
            r.run_volume(
                &cbz,
                &meta("A"),
                &tmp.path().join("A.mokuro"),
                &|_| {},
                &CancelToken::new(),
            )
        });
        let b = s.spawn(|| {
            r.run_volume(
                &cbz,
                &meta("B"),
                &tmp.path().join("B.mokuro"),
                &|_| {},
                &CancelToken::new(),
            )
        });
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert_eq!((a.pages, b.pages), (4, 4));
    let pages_of = |p: &str| -> serde_json::Value {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(p)).unwrap()).unwrap();
        v["pages"].clone()
    };
    assert_eq!(pages_of("A.mokuro"), pages_of("B.mokuro"));
    assert_eq!(pages_of("A.mokuro"), v["pages"]);
}

#[test]
#[ignore = "needs the model files"]
fn refusals_before_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    let err = p.open(&spec("hayai-nova", "bf16")).err().unwrap();
    assert!(err.starts_with("precision not available here"), "{err}");
    // `Backend::Cpu`: paddle-manga runs on a GPU only.
    let err = p.open(&spec("paddle-manga", "fp16")).err().unwrap();
    assert!(err.starts_with("paddle-manga needs a GPU"), "{err}");
    let err = p.open(&spec("mokuro", "auto")).err().unwrap();
    assert!(err.contains("unknown engine mokuro"), "{err}");
    let mut s = spec("ppocr-manga", "auto");
    s.detector = Some("ctd".into());
    assert!(p.open(&s).err().unwrap().contains("unknown detector ctd"));
}

/// paddle-manga runs on a GPU only: on a machine whose devices are the CPU alone
/// (`ocr.backend: cpu` here) a session fails at once with the clear error, before any
/// model is fetched or loaded (none is on disk here), whatever its precision mode; and
/// it is neither offered by `describe` nor resolvable for a download.
#[test]
fn paddle_manga_on_a_cpu_only_host_fails_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    let needs = "paddle-manga needs a GPU (NVIDIA CUDA or AMD ROCm); use hayai-nova on the CPU";
    for mode in ["auto-accuracy", "auto-speed", "fp32", "bf16"] {
        let err = p.open(&spec("paddle-manga", mode)).err().unwrap();
        assert_eq!(err, needs, "{mode}");
    }
    let mut pinned = spec("paddle-manga", "fp32");
    pinned
        .pools
        .stage_device
        .insert("engine".into(), "cpu".into());
    assert!(p.open(&pinned).err().unwrap().starts_with(needs));
    assert_eq!(p.device_for("paddle-manga").unwrap_err(), needs);
    assert!(p.device_for("hayai-nova").is_ok());
    assert!(
        !p.describe()
            .catalog
            .engines
            .contains(&"paddle-manga".to_string())
    );
    #[cfg(feature = "torch")]
    {
        assert!(p.need_for("paddle-manga", "auto-accuracy").is_err());
        for (engine, r) in p.prefetch_rows(&[("paddle-manga", "auto-accuracy")]) {
            assert_eq!(engine, "paddle-manga");
            assert!(r.is_err());
        }
    }
    assert!(
        p.recognizer_for("paddle-manga", bunko_vlm::Precision::Fp32, "cpu", 512)
            .err()
            .unwrap()
            .starts_with(needs)
    );
}
