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
        .map(|n| n.unwrap().into_owned())
        .filter(|n| n.ends_with(".webp") || n.ends_with(".jpg") || n.ends_with(".png"))
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
        Some("d65838996a00ef6b1c73ca3a700d7a8c5fbbef8a")
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
    assert_eq!(
        v["ocr_engine"]["revision"],
        bunko_layout::sidecar::engine_revision("ppocr-manga")
    );
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

/// No backend pack: nar-small opens on the CPU (its ONNX graph) and the sidecar names
/// our export set of that graph.
#[cfg(feature = "nc-models")]
#[test]
#[ignore = "needs the nar-small model files (MOKURO_MODELS_DIR)"]
fn nar_without_a_pack_reads_on_the_cpu() {
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    let runner = p.open(&spec("nar-small", "auto-accuracy")).unwrap();
    let ready = runner.ready();
    assert_eq!(
        ready.stage_device.get("engine").map(String::as_str),
        Some("cpu")
    );
    assert_eq!(
        ready
            .weights
            .get("Gnathonic/mokuro-bunko")
            .map(String::as_str),
        Some("textbox-graphs-v1")
    );
    assert_eq!(
        ready
            .weights
            .get("muscgab/manga-ocr-nar-small")
            .map(String::as_str),
        Some("8cc14c152ccb21da3aa88fa51a92766537c12da2")
    );
    assert_eq!(ready.precision.as_deref(), Some("fp32"));
}

/// With a backend pack that reads text boxes and a nar-small package for the GPU
/// (`MOKURO_TORCH_PACK`, `MOKURO_TORCH_MODELS_DIR`), nar-small opens on the GPU, the
/// sidecar names the text-box torch release, and a volume reads the same text as the
/// CPU route (the engine stage forced to the CPU in the same process).
#[cfg(feature = "nc-models")]
#[test]
#[ignore = "needs a backend pack with bt_read_chars, a nar-small GPU package, the model files and a sample archive"]
fn nar_on_a_gpu_reads_as_the_cpu_does() {
    let Some(src) = sample() else {
        eprintln!("no sample archive; skipped");
        return;
    };
    if std::env::var_os("MOKURO_TORCH_PACK").is_none() {
        eprintln!("no MOKURO_TORCH_PACK; skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let p = EnginePipeline::new(EngineConfig::new(tmp.path().join("models"), Backend::Auto));
    // A process opens one pack: a test of `Backend::Cpu` that ran first opened it for
    // the CPU only. Run this one alone (`-- --ignored nar_on_a_gpu`).
    if !p.devices().iter().any(|d| d.id == "gpu:0") {
        eprintln!("the backend pack of this process sees no GPU; skipped (run this test alone)");
        return;
    }
    let gpu = p.open(&spec("nar-small", "auto-accuracy")).unwrap();
    let ready = gpu.ready();
    assert_eq!(
        ready.stage_device.get("engine").map(String::as_str),
        Some("gpu:0"),
        "{ready:?}"
    );
    assert_eq!(
        ready
            .weights
            .get("Gnathonic/mokuro-bunko")
            .map(String::as_str),
        Some("textbox-torch-v1")
    );
    assert_eq!(ready.precision.as_deref(), Some("fp32"));
    let mut on_cpu = spec("nar-small", "auto-accuracy");
    on_cpu
        .pools
        .stage_device
        .insert("engine".into(), "cpu".into());
    let cpu = p.open(&on_cpu).unwrap();
    assert_eq!(
        cpu.ready().stage_device.get("engine").map(String::as_str),
        Some("cpu")
    );
    assert_eq!(
        cpu.ready()
            .weights
            .get("Gnathonic/mokuro-bunko")
            .map(String::as_str),
        Some("textbox-graphs-v1")
    );

    let cbz = tmp.path().join("Vol.cbz");
    write_cbz(&cbz, &real_pages(&src, 4));
    let texts = |runner: &dyn bunko_processor::VolumeRunner, name: &str| -> Vec<String> {
        let out = tmp.path().join(name);
        runner
            .run_volume(&cbz, &meta("Vol"), &out, &|_| {}, &CancelToken::new())
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        v["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p["blocks"].as_array().unwrap().clone())
            .flat_map(|b| b["lines"].as_array().unwrap().clone())
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    };
    let (g, c) = (texts(&*gpu, "gpu.mokuro"), texts(&*cpu, "cpu.mokuro"));
    assert!(g.len() > 20, "{} lines", g.len());
    assert_eq!(g, c);
    eprintln!("{} lines read alike on the GPU and the CPU", g.len());
}

/// With a backend pack that has `bt_detect_blocks` and an AnimeText package for the GPU
/// (`MOKURO_TORCH_PACK`, `MOKURO_TORCH_MODELS_DIR`), plus nar-small's CPU files and the
/// AnimeText ONNX file (`MOKURO_MODELS_DIR`, downloads off), a nar-small row on
/// AnimeText with its engine stage on the CPU detects on the GPU: the sidecar names the
/// detector's torch release beside nar-small's graph set, and a volume gets the same
/// blocks and text as with the detect stage pinned to the CPU.
#[cfg(all(feature = "nc-models", feature = "gpl-models"))]
#[test]
#[ignore = "needs a backend pack with bt_detect_blocks, an AnimeText GPU package, the model files and a sample archive"]
fn animetext_on_a_gpu_detects_as_the_cpu_does() {
    let Some(src) = sample() else {
        eprintln!("no sample archive; skipped");
        return;
    };
    if std::env::var_os("MOKURO_TORCH_PACK").is_none() {
        eprintln!("no MOKURO_TORCH_PACK; skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let p = EnginePipeline::new(EngineConfig::new(tmp.path().join("models"), Backend::Auto));
    // A process opens one pack (see nar_on_a_gpu_reads_as_the_cpu_does): run alone.
    if !p.devices().iter().any(|d| d.id == "gpu:0") {
        eprintln!("the backend pack of this process sees no GPU; skipped (run this test alone)");
        return;
    }
    let row = |detect: Option<&str>| {
        let mut s = spec("nar-small", "auto-accuracy");
        s.detector = Some("animetext-blocks".into());
        s.pools.stage_device.insert("engine".into(), "cpu".into());
        if let Some(d) = detect {
            s.pools.stage_device.insert("detect".into(), d.into());
        }
        s
    };
    let export = |runner: &dyn bunko_processor::VolumeRunner| {
        runner
            .ready()
            .weights
            .get("Gnathonic/mokuro-bunko")
            .cloned()
    };
    let gpu = p.open(&row(None)).unwrap();
    assert_eq!(
        export(&*gpu).as_deref(),
        Some("detector-torch-v1+textbox-graphs-v1"),
        "{:?}",
        gpu.ready()
    );
    let cpu = p.open(&row(Some("cpu"))).unwrap();
    assert_eq!(export(&*cpu).as_deref(), Some("textbox-graphs-v1"));
    // The plan follows where the detector loaded: a GPU detect stage of two workers, and
    // the CPU pool gives up only its host side.
    let (gr, cr) = (gpu.ready(), cpu.ready());
    assert_eq!(gr.stage_device["detect"], "gpu:0", "{gr:?}");
    assert_eq!(
        gr.stage_workers["detect"],
        bunko_engines::plan::DETECT_GPU_WORKERS
    );
    assert_eq!(cr.stage_device["detect"], "cpu", "{cr:?}");
    assert!(
        gr.stage_workers["engine"] >= cr.stage_workers["engine"],
        "{gr:?} {cr:?}"
    );
    eprintln!("GPU detect: {}\nCPU detect: {}", gr.pipeline, cr.pipeline);

    let cbz = tmp.path().join("Vol.cbz");
    write_cbz(&cbz, &real_pages(&src, 4));
    let blocks =
        |runner: &dyn bunko_processor::VolumeRunner, name: &str| -> Vec<serde_json::Value> {
            let out = tmp.path().join(name);
            runner
                .run_volume(&cbz, &meta("Vol"), &out, &|_| {}, &CancelToken::new())
                .unwrap();
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
            v["pages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|p| p["blocks"].as_array().unwrap().clone())
                .map(|b| serde_json::json!([b["box"], b["lines"]]))
                .collect()
        };
    let (g, c) = (blocks(&*gpu, "gpu.mokuro"), blocks(&*cpu, "cpu.mokuro"));
    assert!(g.len() > 10, "{} blocks", g.len());
    assert_eq!(g, c);
    eprintln!(
        "{} blocks detected and read alike on the GPU and the CPU",
        g.len()
    );
}

#[test]
#[ignore = "needs the model files"]
fn refusals_before_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    // The precision refusals hold only without a backend pack: with one (say
    // `MOKURO_TORCH_PACK`, set for `torch_loader` in the same run) both rows open.
    if std::env::var_os("MOKURO_TORCH_PACK").is_none()
        && std::env::var_os(bunko_engines::BACKENDS_DIR_ENV).is_none()
    {
        let err = p.open(&spec("hayai-nova", "bf16")).err().unwrap();
        assert!(err.starts_with("precision not available here"), "{err}");
        let err = p.open(&spec("paddle-manga", "fp16")).err().unwrap();
        assert!(err.contains("fp16 not supported"), "{err}");
    } else {
        eprintln!("a backend pack is configured: precision refusals not checked");
    }
    let err = p.open(&spec("mokuro", "auto")).err().unwrap();
    assert!(err.contains("unknown engine mokuro"), "{err}");
    let mut s = spec("ppocr-manga", "auto");
    s.detector = Some("ctd".into());
    assert!(p.open(&s).err().unwrap().contains("unknown detector ctd"));
}

/// No backend pack: a nar-small row's need is the CPU (its ONNX route) at fp32, with no
/// package targets, and says why it is not on a GPU.
#[cfg(all(feature = "nc-models", feature = "torch"))]
#[test]
fn a_text_box_need_without_a_pack_is_the_cpu() {
    let tmp = tempfile::tempdir().unwrap();
    let p = pipeline(tmp.path());
    let need = p.need_for("nar-small", "auto-accuracy").unwrap();
    assert_eq!(
        (need.device.as_str(), need.precision),
        ("cpu", bunko_vlm::Precision::Fp32)
    );
    assert!(need.targets.is_empty());
    assert!(need.fallback.is_some());
    // Its plan is the ONNX route: the graph and the weights it binds, and the vocabulary.
    let plan = p.package_plan("nar-small", "auto-accuracy").unwrap();
    assert_eq!(plan.target.as_deref(), Some("onnx"));
    assert_eq!(
        plan.package_ids,
        ["nar-small/graph-r16", "nar-small/weights-r16"]
    );
    assert_eq!(plan.host_ids, ["nar-small/vocabulary"]);
    if std::env::var_os("MOKURO_MODELS_DIR").is_none() {
        let st = p.package_status_for("nar-small", "auto-accuracy").unwrap();
        assert!(st.package.is_none() && !st.ready());
        assert_eq!(st.missing_host_files, ["nar-small/vocabulary"]);
    }
}
