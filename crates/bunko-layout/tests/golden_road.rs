//! Golden: the reconciled road end to end (plan, engine reads, merge, second
//! reads, verdicts, seam trim, layout, raw dump, review lines) on real pages,
//! the 0.5.2 fixture pages and page 129 with the engines' real reads; then
//! volume sidecars (runner bytes) and the server's normalised rewrite.

mod common;

use bunko_layout::json::{Separators, Value};
use bunko_layout::layout::layout_page;
use bunko_layout::records::{RawLine, RawPage};
use bunko_layout::road::EngineRoad;
use bunko_layout::sidecar::{
    LayerStamp, MOKURO_FORMAT_VERSION, Normalization, OcrEngine, Page, VolumeHeader, build_volume,
    derive_series_name, deterministic_uuid, normalize_sidecar_file, title_uuid, write_sidecar,
};
use common::*;

struct RoadInput {
    width: i64,
    height: i64,
    detector: Value,
    lines: Vec<RawLine>,
}

fn road_input(case: &Value, reals: &[(String, RawPage)]) -> RoadInput {
    let page_v: Value = match s(get(case, "ref")) {
        "real" => {
            let name = s(get(case, "page_name"));
            let p = &reals.iter().find(|(n, _)| n == name).expect("real page").1;
            return RoadInput {
                width: p.width,
                height: p.height,
                detector: p.detector.clone().unwrap_or_else(Value::object),
                lines: p.lines.clone(),
            };
        }
        "fixture" => fixture_page(s(get(case, "page_name"))),
        _ => get(case, "page").clone(),
    };
    let lines = arr(get(&page_v, "lines"))
        .iter()
        .map(line_as_python)
        .collect();
    RoadInput {
        width: f(get(&page_v, "width")) as i64,
        height: f(get(&page_v, "height")) as i64,
        detector: page_v
            .get("detector")
            .filter(|d| d.truthy())
            .cloned()
            .unwrap_or_else(Value::object),
        lines,
    }
}

fn reads(v: &Value, targets: &[usize]) -> Vec<String> {
    targets
        .iter()
        .map(|t| {
            v.get(&t.to_string())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// Run one road case; return the finished page (for the volume tests) and the
/// differences found.
fn run_road(case: &Value, reals: &[(String, RawPage)]) -> (Page, Vec<String>) {
    let name = s(get(case, "name"));
    let want = get(case, "expect");
    let mut errs = Vec::new();
    let RoadInput {
        width,
        height,
        detector,
        mut lines,
    } = road_input(case, reals);

    let first = layout_page(&RawPage::new(width, height, lines.clone()).rounded());
    let road = EngineRoad::plan(&lines, &first);
    let want_targets: Vec<usize> = arr(get(want, "targets"))
        .iter()
        .map(|t| i(t) as usize)
        .collect();
    if road.targets != want_targets {
        errs.push(format!(
            "{name}: targets {:?} != {want_targets:?}",
            road.targets
        ));
    }
    if !same(road.pitch, f(get(want, "pitch"))) {
        errs.push(format!(
            "{name}: pitch {} != {}",
            road.pitch,
            f(get(want, "pitch"))
        ));
    }
    let caps: Vec<i64> = arr(get(want, "first_caps")).iter().map(i).collect();
    if !caps.is_empty() && road.token_caps() != caps {
        errs.push(format!(
            "{name}: token caps {:?} != {caps:?}",
            road.token_caps()
        ));
    }

    let texts = reads(get(case, "first_reads"), &road.targets);
    let mut settled = road.reconcile_first(&lines, &texts);
    if let Some(second_v) = case
        .get("second_reads")
        .filter(|v| !matches!(v, Value::Null))
    {
        let doubted = road.doubted(&settled);
        let doubted_lines: Vec<usize> = doubted.iter().map(|&k| road.targets[k]).collect();
        let second = reads(second_v, &doubted_lines);
        road.settle_second(&mut settled, &doubted, &second);
    }
    road.apply(&mut lines, &mut settled, width, height);
    let want_settled = arr(get(want, "settled"));
    for (k, (g, w)) in settled.iter().zip(want_settled).enumerate() {
        if let Err(e) = check_reconciled(g, w) {
            errs.push(format!(
                "{name}: settled[{k}] (line {}):\n{e}",
                road.targets[k]
            ));
        }
    }
    for (idx, (g, w)) in lines.iter().zip(arr(get(want, "lines"))).enumerate() {
        if g.text != s(get(w, "text")) || !same(g.conf, f(get(w, "conf"))) {
            errs.push(format!(
                "{name}: line {idx} = ({:?}, {}) != {}",
                g.text,
                g.conf,
                short(w)
            ));
        }
    }
    let done = road.finish(
        &lines,
        &settled,
        width,
        height,
        Some(detector),
        MOKURO_FORMAT_VERSION,
    );
    let page_json = done.page.to_value(None).dumps(Separators::Default);
    if page_json != s(get(want, "page_json")) {
        errs.push(format!(
            "{name}: page json\n got {page_json}\nwant {}",
            s(get(want, "page_json"))
        ));
    }
    let raw_json = done.raw.dumps(Separators::Default);
    if raw_json != s(get(want, "raw_json")) {
        let (g, w) = (raw_json.as_str(), s(get(want, "raw_json")));
        let at = g
            .bytes()
            .zip(w.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(g.len().min(w.len()));
        let window = |t: &str| -> String {
            String::from_utf8_lossy(&t.as_bytes()[at.saturating_sub(120)..(at + 120).min(t.len())])
                .into_owned()
        };
        errs.push(format!(
            "{name}: raw dump differs at byte {at}\n got ...{}\nwant ...{}",
            window(g),
            window(w)
        ));
    }
    let doubtful =
        Value::Array(done.doubtful.clone().unwrap_or_default()).dumps(Separators::Default);
    if doubtful != s(get(want, "doubtful_json")) {
        errs.push(format!(
            "{name}: review lines\n got {doubtful}\nwant {}",
            s(get(want, "doubtful_json"))
        ));
    }
    (done.page, errs)
}

#[test]
fn reconciled_road_matches_python() {
    let cases = load("cases/road.json.gz");
    let reals = real_pages();
    let mut errs = Vec::new();
    for case in arr(&cases) {
        errs.extend(run_road(case, &reals).1);
    }
    assert!(
        arr(&cases).len() >= 50,
        "only {} road cases",
        arr(&cases).len()
    );
    assert!(
        errs.is_empty(),
        "{} differences:\n{}",
        errs.len(),
        errs.join("\n\n")
    );
}

#[test]
fn volume_sidecars_and_normalization_match_python() {
    let road_cases = load("cases/road.json.gz");
    let reals = real_pages();
    // The page dicts in the generator's `pages_out` order (seam cases excluded).
    let pages: Vec<Page> = arr(&road_cases)
        .iter()
        .filter(|c| !s(get(c, "name")).starts_with("seam-"))
        .map(|c| run_road(c, &reals).0)
        .collect();
    let v = load("cases/sidecar.json.gz");
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut errs = Vec::new();
    for case in arr(get(&v, "volumes")) {
        let name = s(get(case, "name"));
        let h = get(case, "header");
        let ocr_engine = match get(case, "engine") {
            Value::Null => None,
            e => {
                let generator = e.get("generator").and_then(Value::as_str);
                let mut meta = OcrEngine::new(
                    s(get(case, "normalize").get("engine").expect("engine")),
                    generator,
                    i(get(e, "patches")),
                );
                meta.weights = arr(get(e, "weights"))
                    .iter()
                    .map(|kv| (s(&arr(kv)[0]).to_string(), s(&arr(kv)[1]).to_string()))
                    .collect();
                meta.precision = e
                    .get("precision")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some(meta.to_value())
            }
        };
        let header = VolumeHeader {
            version: s(get(h, "version")).to_string(),
            title: s(get(h, "title")).to_string(),
            title_uuid: s(get(h, "title_uuid")).to_string(),
            volume: s(get(h, "volume")).to_string(),
            volume_uuid: s(get(h, "volume_uuid")).to_string(),
            ocr_engine,
        };
        let chosen: Vec<(String, Page)> = arr(get(case, "page_indices"))
            .iter()
            .zip(arr(get(case, "img_paths")))
            .map(|(k, p)| (s(p).to_string(), pages[i(k) as usize].clone()))
            .collect();
        let volume = build_volume(&header, &chosen);
        let path = tmp.path().join(format!("{name}.mokuro"));
        write_sidecar(&path, &volume).expect("write");
        let runner_bytes = std::fs::read_to_string(&path).expect("read");
        if !b(get(case, "runner_modified")) && runner_bytes != s(get(case, "runner_bytes")) {
            errs.push(format!(
                "{name}: runner bytes\n got {}\nwant {}",
                &runner_bytes,
                s(get(case, "runner_bytes"))
            ));
        }
        // normalise the bytes Python's runner wrote
        std::fs::write(&path, s(get(case, "runner_bytes"))).expect("write");
        let n = get(case, "normalize");
        let cbz = std::path::Path::new(s(get(n, "cbz")));
        let series = derive_series_name(
            cbz,
            std::path::Path::new(s(get(n, "library"))),
            std::path::Path::new(s(get(n, "inbox"))),
        );
        if series != s(get(n, "series_name")) {
            errs.push(format!(
                "{name}: series {series:?} != {:?}",
                s(get(n, "series_name"))
            ));
        }
        let norm = Normalization {
            series_name: series,
            volume: cbz
                .file_stem()
                .expect("stem")
                .to_string_lossy()
                .into_owned(),
            volume_uuid: s(get(n, "volume_uuid")).to_string(),
            stamp: (!b(get(n, "primary"))).then(|| LayerStamp {
                engine: s(get(n, "engine")).to_string(),
                generator: s(get(n, "generator")).to_string(),
                generation: s(get(n, "generation")).to_string(),
            }),
        };
        assert!(normalize_sidecar_file(&path, &norm).expect("normalize"));
        let normalized = std::fs::read_to_string(&path).expect("read");
        if normalized != s(get(case, "normalized_bytes")) {
            errs.push(format!(
                "{name}: normalized\n got {normalized}\nwant {}",
                s(get(case, "normalized_bytes"))
            ));
        }
        let leftover = std::fs::read_dir(tmp.path()).expect("ls").filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        });
        assert_eq!(leftover.count(), 0, "temporary file left behind");
    }
    for u in arr(get(&v, "uuids")) {
        let value = s(get(u, "value"));
        if deterministic_uuid(value) != s(get(u, "deterministic")) {
            errs.push(format!(
                "deterministic_uuid({value:?}) = {}",
                deterministic_uuid(value)
            ));
        }
        if title_uuid(value) != s(get(u, "uuid5")) {
            errs.push(format!("uuid5({value:?}) = {}", title_uuid(value)));
        }
    }
    assert!(arr(get(&v, "volumes")).len() >= 8);
    assert!(
        errs.is_empty(),
        "{} differences:\n{}",
        errs.len(),
        errs.join("\n\n")
    );
}
