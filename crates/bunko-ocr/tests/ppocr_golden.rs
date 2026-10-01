//! OCR parity with the Python 0.5.2 `ppocr-manga` reader on real pages.
//!
//! Fixtures: `tests/golden/ppocr/<id>.json` (made by `gen_ppocr_golden.py` with
//! onnxruntime 1.30 / cv2 5.0 / Pillow 12.3). Page images come from the sample
//! archives in `~/Downloads` (never written to); the layout decisions the page
//! passes need come from the Python layout via `layout_oracle.py` until
//! `bunko-layout` exists. Heavy and environment-bound, hence `#[ignore]`:
//!
//! ```text
//! CARGO_TARGET_DIR=target/agent-bunko-ocr cargo test -p bunko-ocr --release \
//!     --test ppocr_golden -- --ignored --nocapture
//! ```
//!
//! Env: `BUNKO_REF_PYTHON` (default `~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python`),
//! `BUNKO_PPOCR_MODELS` (default: the pinned Hugging Face cache snapshot),
//! `BUNKO_GOLDEN_THREADS` (ORT intra-op threads, default 4 like the fixtures),
//! `BUNKO_GOLDEN_PREP` (crop/tensor threads, default 1 — Python's is 1),
//! `BUNKO_GOLDEN_ONLY` (comma-separated page ids), `BUNKO_GOLDEN_TARGETS`
//! (execution providers in preference order, default `cpu`), `BUNKO_GOLDEN_NOSPIN`
//! (disable ORT thread spinning, for CPU-time comparisons on a loaded host).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use bunko_ocr::image::{BgrImage, decode_bgr};
use bunko_ocr::lines::{RawLine, RawPage};
use bunko_ocr::models::{Manifest, ModelStore, StoreOptions};
use bunko_ocr::pages::ArchivePages;
use bunko_ocr::ppocr::geometry::{Quad, quad_iou};
use bunko_ocr::ppocr::{LayoutHooks, PpOcr, PpocrPageReader, page_to_json};
use bunko_ocr::runtime::RuntimeOptions;
use serde_json::Value;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

struct Oracle {
    _child: Child,
    io: RefCell<(ChildStdin, BufReader<ChildStdout>)>,
    spent: Cell<Duration>,
}

impl Oracle {
    fn start(python: &Path) -> Self {
        let mut child = Command::new(python)
            .arg(golden_dir().join("layout_oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start layout oracle");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            _child: child,
            io: RefCell::new((stdin, stdout)),
            spent: Cell::new(Duration::ZERO),
        }
    }

    fn ask(&self, op: &str, raw: &RawPage) -> Value {
        let t = Instant::now();
        let mut io = self.io.borrow_mut();
        let req = serde_json::json!({"op": op, "raw": raw});
        writeln!(io.0, "{req}").expect("write");
        io.0.flush().expect("flush");
        let mut line = String::new();
        io.1.read_line(&mut line).expect("read");
        self.spent.set(self.spent.get() + t.elapsed());
        serde_json::from_str(&line).expect("oracle json")
    }
}

fn indices(v: &Value) -> BTreeSet<usize> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64())
                .map(|x| x as usize)
                .collect()
        })
        .unwrap_or_default()
}

impl LayoutHooks for Oracle {
    type Layout = (BTreeSet<usize>, BTreeSet<usize>);

    fn column_pieces(&self, raw: &RawPage) -> Vec<Vec<usize>> {
        let v = self.ask("pieces", raw);
        v["pieces"]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .map(|g| indices(g).into_iter().collect())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn layout(&self, raw: &RawPage) -> Self::Layout {
        let v = self.ask("layout", raw);
        (indices(&v["ruby"]), indices(&v["bodies"]))
    }

    fn ruby_lines(&self, l: &Self::Layout) -> BTreeSet<usize> {
        l.0.clone()
    }

    fn body_lines(&self, l: &Self::Layout) -> BTreeSet<usize> {
        l.1.clone()
    }
}

fn load_image(entry: &Value) -> Option<BgrImage> {
    let archive = home().join("Downloads").join(entry["archive"].as_str()?);
    if !archive.is_file() {
        return None;
    }
    let mut pages = ArchivePages::open(&archive, None).ok()?;
    let list: Vec<String> = pages.pages().to_vec();
    let mut decode =
        |i: usize| decode_bgr(&pages.read(&list[i]).expect("read page")).expect("decode");
    if let Some(rows) = entry.get("compose").and_then(|c| c.as_array()) {
        let rows: Vec<BgrImage> = rows
            .iter()
            .map(|r| {
                let parts: Vec<BgrImage> = r
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| decode(i.as_u64().unwrap_or(0) as usize))
                    .collect();
                BgrImage::hconcat(&parts).expect("same height")
            })
            .collect();
        return BgrImage::vconcat(&rows);
    }
    let img = decode(entry["page_index"].as_u64()? as usize);
    Some(match entry.get("crop").and_then(|c| c.as_array()) {
        Some(c) => {
            let c: Vec<usize> = c.iter().map(|v| v.as_u64().unwrap_or(0) as usize).collect();
            img.crop(c[0], c[1], c[2], c[3])
        }
        None => img,
    })
}

fn quad_of(l: &RawLine) -> Quad {
    l.quad.map(|p| [p[0] as f32, p[1] as f32])
}

#[derive(Default, Debug, Clone, Copy)]
struct Tally {
    python: usize,
    rust: usize,
    exact: usize,
    text_diff: usize,
    geom_diff: usize,
    missing: usize,
    extra: usize,
    /// Matched lines whose whole record (rounded quad, score, conf, char_confs,
    /// angle) is identical too.
    identical: usize,
}

impl Tally {
    fn add(&mut self, o: &Tally) {
        self.python += o.python;
        self.rust += o.rust;
        self.exact += o.exact;
        self.text_diff += o.text_diff;
        self.geom_diff += o.geom_diff;
        self.missing += o.missing;
        self.extra += o.extra;
        self.identical += o.identical;
    }
}

/// Match lines by IoU (greedy, best first) and classify every difference.
fn compare(id: &str, stage: &str, py: &RawPage, rs: &RawPage, notes: &mut Vec<String>) -> Tally {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (i, a) in py.lines.iter().enumerate() {
        for (j, b) in rs.lines.iter().enumerate() {
            let iou = quad_iou(&quad_of(a), &quad_of(b));
            if iou > 0.3 {
                pairs.push((iou, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut used_py = vec![false; py.lines.len()];
    let mut used_rs = vec![false; rs.lines.len()];
    let mut t = Tally {
        python: py.lines.len(),
        rust: rs.lines.len(),
        ..Default::default()
    };
    for (iou, i, j) in pairs {
        if used_py[i] || used_rs[j] {
            continue;
        }
        used_py[i] = true;
        used_rs[j] = true;
        let (a, b) = (&py.lines[i], &rs.lines[j]);
        if a.text != b.text {
            t.text_diff += 1;
            notes.push(format!(
                "{id} [{stage}] TEXT  py «{}» rs «{}» (iou {iou:.4}, conf {} / {})",
                a.text, b.text, a.conf, b.conf
            ));
        } else if iou < 0.98 {
            t.geom_diff += 1;
            notes.push(format!("{id} [{stage}] GEOM  «{}» iou {iou:.4}", a.text));
        } else {
            t.exact += 1;
            if a == b {
                t.identical += 1;
            }
        }
    }
    for (i, u) in used_py.iter().enumerate() {
        if !u {
            t.missing += 1;
            notes.push(format!(
                "{id} [{stage}] MISSING «{}» score {} quad {:?}",
                py.lines[i].text, py.lines[i].score, py.lines[i].quad[0]
            ));
        }
    }
    for (j, u) in used_rs.iter().enumerate() {
        if !u {
            t.extra += 1;
            notes.push(format!(
                "{id} [{stage}] EXTRA «{}» score {} quad {:?}",
                rs.lines[j].text, rs.lines[j].score, rs.lines[j].quad[0]
            ));
        }
    }
    t
}

/// User + system CPU seconds of this process so far (all threads).
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) / 100.0
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "needs ~/Downloads samples, the PP-OCR models and the Python reference env"]
fn ppocr_matches_python_052() {
    let python = std::env::var_os("BUNKO_REF_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache/mokuro-bunko-demo/ref052-ocr/bin/python"));
    let models = std::env::var_os("BUNKO_PPOCR_MODELS").map(PathBuf::from).unwrap_or_else(|| {
        home().join(".cache/huggingface/hub/models--Kellenok--PP-OCRv6_manga/snapshots/ba1d479e8a61a20e8318c9758c73fbbbd290b98d")
    });
    let tmp = tempfile::tempdir().expect("tmp");
    let store = ModelStore::new(
        StoreOptions {
            root: tmp.path().join("models"),
            override_dir: Some(models),
            download: false,
        },
        Manifest::builtin(),
    );
    let files = store.ppocr().expect("models");
    assert!(files.pinned, "model files do not match the pinned manifest");
    let threads = env_usize("BUNKO_GOLDEN_THREADS", 4);
    let prep = env_usize("BUNKO_GOLDEN_PREP", 1);
    let targets = std::env::var("BUNKO_GOLDEN_TARGETS").unwrap_or_else(|_| "cpu".into());
    let targets = targets
        .split(',')
        .map(|t| t.parse().expect("target"))
        .collect();
    let spinning = std::env::var("BUNKO_GOLDEN_NOSPIN").is_err();
    let opts = RuntimeOptions {
        intra_threads: threads,
        targets,
        spinning,
        ..Default::default()
    };
    let engine = PpOcr::load(&files.detector, &files.recognizer, &files.dictionary, &opts)
        .expect("load")
        .with_prep_threads(prep);
    println!(
        "detector on {} ({:?}), recognizer on {} ({:?})",
        engine.detector().target(),
        engine.detector().fallback_reason(),
        engine.recognizer().target(),
        engine.recognizer().fallback_reason()
    );
    let reader = PpocrPageReader::new(engine, files.pinned);
    let oracle = Oracle::start(&python);
    let only: Vec<String> = std::env::var("BUNKO_GOLDEN_ONLY")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();

    let entries: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(golden_dir().join("ppocr_pages.json")).expect("pages"),
    )
    .expect("json");
    let (mut first_total, mut final_total) = (Tally::default(), Tally::default());
    let mut notes: Vec<String> = Vec::new();
    let mut rs_time = (0.0f64, 0.0f64);
    let mut rs_cpu = 0.0f64;
    let mut ran = 0;
    // Warm up both sessions once, as bench_ppocr_python.py does.
    if let Some(first) = entries.first()
        && let Ok(text) = std::fs::read_to_string(
            golden_dir().join(format!("ppocr/{}.json", first["id"].as_str().unwrap_or(""))),
        )
        && let Some(img) = serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(load_image)
    {
        reader.engine().read_page(&img).expect("warm-up");
    }
    println!(
        "{:<26} {:>5} {:>5}  {:>12} {:>12}",
        "page", "py", "rs", "read_page s", "read_lines s"
    );
    for entry in &entries {
        let id = entry["id"].as_str().unwrap_or("");
        if !only.is_empty() && !only.iter().any(|o| o == id) {
            continue;
        }
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string(golden_dir().join(format!("ppocr/{id}.json")))
                .expect("fixture"),
        )
        .expect("json");
        let Some(img) = load_image(&fixture) else {
            println!("{id}: sample archive missing, skipped");
            continue;
        };
        let (w, h) = (img.width(), img.height());
        let py_first: RawPage = serde_json::from_value(fixture["first"].clone()).expect("first");
        let py_final: RawPage = serde_json::from_value(fixture["final"].clone()).expect("final");

        let c0 = cpu_seconds();
        let t0 = Instant::now();
        let (lines, info) = reader.engine().read_page(&img).expect("read_page");
        let t_page = t0.elapsed().as_secs_f64();
        rs_cpu += cpu_seconds() - c0;
        let rs_first = page_to_json(&lines, w, h, Some(info));
        oracle.spent.set(Duration::ZERO);
        let t1 = Instant::now();
        let read = reader.read_lines(&img, &oracle).expect("read_lines");
        let t_lines = t1.elapsed().as_secs_f64() - oracle.spent.get().as_secs_f64();
        let rs_final = read.raw(w, h);

        let a = compare(id, "first", &py_first, &rs_first, &mut notes);
        let b = compare(id, "final", &py_final, &rs_final, &mut notes);
        first_total.add(&a);
        final_total.add(&b);
        rs_time.0 += t_page;
        rs_time.1 += t_lines;
        ran += 1;
        println!(
            "{id:<26} {:>5} {:>5}  {t_page:>12.3} {t_lines:>12.3}",
            py_final.lines.len(),
            rs_final.lines.len()
        );
        if rs_final.detector != py_final.detector {
            notes.push(format!(
                "{id} detector info differs: py {:?} rs {:?}",
                py_final.detector, rs_final.detector
            ));
        }
    }
    println!("\n--- differences ---");
    for n in &notes {
        println!("{n}");
    }
    let rate = |t: &Tally| t.exact as f64 / t.python.max(t.rust).max(1) as f64;
    println!("\npages: {ran}  threads: {threads}  prep: {prep}");
    println!(
        "first (read_page):  {first_total:?}  exact {:.2}%",
        100.0 * rate(&first_total)
    );
    println!(
        "final (read_lines): {final_total:?}  exact {:.2}%",
        100.0 * rate(&final_total)
    );
    // Compare with bench_ppocr_python.py run under the same machine load.
    println!(
        "TOTAL threads={threads} pages={ran} read_page {:.2}s read_lines {:.2}s; read_page CPU {:.2}s",
        rs_time.0, rs_time.1, rs_cpu
    );
    assert!(ran > 0, "no pages ran");
    assert!(
        rate(&final_total) >= 0.99,
        "final exact-match rate {:.4} < 0.99",
        rate(&final_total)
    );
}
