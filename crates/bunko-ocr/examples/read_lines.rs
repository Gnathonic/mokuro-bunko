//! Detect and read the lines of some archive pages with one line detector, and write
//! each page's raw lines (`ppocr-lines/1`) as JSON, with an overlay image on request.
//! For looking at what a detector finds; no layout runs (no joins, no ruby).
//!
//! ```text
//! cargo run -p bunko-ocr --release --features gpl-models --example read_lines -- \
//!     <ppocr-manga|ctd-lines> <archive.cbz> <out dir> [--pages 5,20,50] [--storage DIR] \
//!     [--threads N] [--overlay]
//! ```
//!
//! `--pages` are indices into the archive's page list (default: every page). Model
//! files are kept in `<storage>/models` (default: a folder under the temp directory) and
//! downloaded there when missing.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use bunko_ocr::image::{BgrImage, decode_bgr};
use bunko_ocr::models::{Manifest, ModelStore, StoreOptions};
use bunko_ocr::pages::ArchivePages;
use bunko_ocr::ppocr::{NoLayout, PpOcr, PpocrPageReader, Quad};
use bunko_ocr::runtime::RuntimeOptions;

fn engine(detector: &str, store: &ModelStore, opts: &RuntimeOptions) -> Result<PpOcr> {
    let files = store.ppocr()?;
    let engine = PpOcr::load(&files.detector, &files.recognizer, &files.dictionary, opts)?;
    match detector {
        "ppocr-manga" => Ok(engine),
        #[cfg(feature = "gpl-models")]
        "ctd-lines" => {
            let file = store.ensure(bunko_ocr::models::CTD_DETECTOR)?;
            let ctd = bunko_ocr::ppocr::ctd::Ctd::load(&file.path, opts)?;
            Ok(engine.with_line_detector(std::sync::Arc::new(ctd)))
        }
        other => bail!("this build has no line detector {other}"),
    }
}

/// The page with every line's quad drawn on it, as a PNG.
fn overlay(page: &BgrImage, quads: &[Quad], path: &Path) -> Result<()> {
    let (w, h) = (page.width() as u32, page.height() as u32);
    let mut rgb: Vec<u8> = page
        .as_raw()
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let mut dot = |x: i64, y: i64| {
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (x, y) = (x + dx, y + dy);
            if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                let i = (y as usize * w as usize + x as usize) * 3;
                rgb[i..i + 3].copy_from_slice(&[255, 0, 60]);
            }
        }
    };
    for q in quads {
        for k in 0..4 {
            let (a, b) = (q[k], q[(k + 1) % 4]);
            let steps = ((b[0] - a[0]).abs().max((b[1] - a[1]).abs()).ceil() as usize).max(1);
            for s in 0..=steps {
                let t = s as f32 / steps as f32;
                dot(
                    (a[0] + (b[0] - a[0]) * t).round() as i64,
                    (a[1] + (b[1] - a[1]) * t).round() as i64,
                );
            }
        }
    }
    image::RgbImage::from_raw(w, h, rgb)
        .context("overlay buffer")?
        .save(path)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        bail!(
            "usage: read_lines <detector> <archive.cbz> <out dir> [--pages a,b,c] [--storage DIR] [--threads N] [--overlay]"
        );
    }
    let (detector, archive, out) = (&args[0], PathBuf::from(&args[1]), PathBuf::from(&args[2]));
    let mut pick: Option<Vec<usize>> = None;
    let mut storage = std::env::temp_dir().join("bunko-ocr-example");
    let mut threads = 4usize;
    let mut draw = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--overlay" => {
                draw = true;
                i += 1;
                continue;
            }
            "--pages" => {
                pick = Some(
                    args.get(i + 1)
                        .context("--pages a,b,c")?
                        .split(',')
                        .map(|p| p.trim().parse())
                        .collect::<std::result::Result<_, _>>()?,
                )
            }
            "--storage" => storage = PathBuf::from(args.get(i + 1).context("--storage DIR")?),
            "--threads" => threads = args.get(i + 1).context("--threads N")?.parse()?,
            other => bail!("unknown flag {other}"),
        }
        i += 2;
    }
    std::fs::create_dir_all(&out)?;
    let store = ModelStore::new(StoreOptions::from_env(&storage), Manifest::builtin());
    let opts = RuntimeOptions {
        intra_threads: threads,
        ..Default::default()
    };
    let reader = PpocrPageReader::new(engine(detector, &store, &opts)?, false);
    let mut pages = ArchivePages::open(&archive, None)?;
    let names: Vec<String> = pages.pages().to_vec();
    let wanted: Vec<usize> = pick.unwrap_or_else(|| (0..names.len()).collect());
    let (mut total, mut seconds) = (0usize, 0.0f64);
    for index in wanted {
        let name = names
            .get(index)
            .with_context(|| format!("no page {index}"))?;
        let page = decode_bgr(&pages.read(name)?)?;
        let t = Instant::now();
        let read = reader.read_lines(&page, &NoLayout)?;
        let took = t.elapsed().as_secs_f64();
        let raw = read.raw(page.width(), page.height());
        let stem = format!("{index:04}");
        std::fs::write(
            out.join(format!("{stem}.json")),
            serde_json::to_string_pretty(&raw)?,
        )?;
        if draw {
            let quads: Vec<Quad> = read.lines.iter().map(|l| l.quad).collect();
            overlay(&page, &quads, &out.join(format!("{stem}.png")))?;
        }
        println!(
            "{stem} {name}: {}x{} {} lines in {took:.2}s",
            page.width(),
            page.height(),
            read.lines.len()
        );
        total += read.lines.len();
        seconds += took;
    }
    println!("{detector}: {total} lines, {seconds:.2}s");
    Ok(())
}
