//! Throughput of the text-box recognizers (and of the AnimeText block detector) when
//! W callers share ONE ONNX Runtime CPU session with T intra-op threads: how W engine
//! (or detect) workers of a processor would share it.
//!
//! ```text
//! cargo run -p bunko-engines --release --features nc-models,gpl-models --example textbox_bench -- \
//!     nar <graph.onnx> <vocabulary.json> <crops-dir> --weights <model.safetensors> [--n 600] [--warm 32] \
//!         [--threads 1,2,4,8] [--workers 1,2,4,8,12,16]
//!     charspot <model.onnx> <vocabulary.json> <crops-dir> [same options]
//!     animetext <yolo12x.onnx> <volume.cbz>... [--n 60] [--warm 4] \
//!         [--threads shipped,per:2,6] [--workers 1,2,4,8]
//!     mixed <nar-graph.onnx> <vocabulary.json> <crops-dir> <yolo12x.onnx> <volume.cbz>... \
//!         --weights <model.safetensors> --engine W:T --detect W:T [--secs 20]
//! ```
//!
//! The inputs are decoded before any timing (crops: every k-th file of the directory
//! in name order, so `--n` of them; pages: every k-th page of the archives). Each
//! session is warmed up with `--warm` items before its runs. Per run it prints the
//! items per second, the wall time, and the process CPU-seconds per item (`getrusage`,
//! so ORT's pool threads count), and how many outputs differ from the first run's
//! (concurrency and thread counts must not change what is read).
//!
//! `--threads` for the detector: `shipped` is the processor's rule (4 + 2 per extra
//! caller), `per:K` is K per caller, a number is that many whatever the callers.
//! `mixed` runs the detector and nar-small side by side for `--secs`, each pool
//! cycling over its inputs, to see what one does to the other on a shared CPU.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bunko_engines::textbox::{Charspot, NarSmall};
use bunko_ocr::runtime::{ExecutionTarget, RuntimeOptions};
use bunko_vlm::Rgb;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// User + system CPU seconds of the whole process so far.
fn cpu_secs() -> f64 {
    #[cfg(unix)]
    {
        // SAFETY: getrusage writes into the zeroed struct it is given.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }
    #[cfg(not(unix))]
    {
        0.0
    }
}

fn opts(threads: usize) -> RuntimeOptions {
    RuntimeOptions {
        intra_threads: threads,
        targets: vec![ExecutionTarget::Cpu],
        copies: 1,
        spinning: false,
    }
}

/// `f` over `0..items` by `workers` threads taking the next index in turn; the results
/// in index order.
fn run_pool<R: Send>(items: usize, workers: usize, f: &(dyn Fn(usize) -> R + Sync)) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Vec<(usize, R)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= items {
                            break mine;
                        }
                        mine.push((i, f(i)));
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all: Vec<(usize, R)> = parts.iter_mut().flat_map(std::mem::take).collect();
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, r)| r).collect()
}

struct Timed<R> {
    out: Vec<R>,
    wall: f64,
    cpu: f64,
}

fn timed<R: Send>(items: usize, workers: usize, f: &(dyn Fn(usize) -> R + Sync)) -> Timed<R> {
    let (t0, c0) = (Instant::now(), cpu_secs());
    let out = run_pool(items, workers, f);
    Timed {
        out,
        wall: t0.elapsed().as_secs_f64(),
        cpu: cpu_secs() - c0,
    }
}

fn list(arg: Option<&String>, default: &str) -> Vec<String> {
    arg.map_or(default, String::as_str)
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn nums(arg: Option<&String>, default: &str) -> Result<Vec<usize>> {
    list(arg, default)
        .iter()
        .map(|s| s.parse().with_context(|| format!("not a number: {s}")))
        .collect()
}

/// `--name value` options and the positional arguments.
fn split_args(args: &[String]) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let (mut pos, mut opt) = (Vec::new(), std::collections::HashMap::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(name) = a.strip_prefix("--") {
            opt.insert(name.to_string(), it.next().cloned().unwrap_or_default());
        } else {
            pos.push(a.clone());
        }
    }
    (pos, opt)
}

/// Every k-th image of `dir` in name order, `n` of them, decoded.
fn load_crops(dir: &Path, n: usize) -> Result<Vec<Rgb>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| dir.display().to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png" || e == "jpg"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no images in {}", dir.display());
    }
    let step = (files.len() / n.max(1)).max(1);
    files
        .iter()
        .step_by(step)
        .take(n)
        .map(|f| {
            let img = image::open(f)
                .with_context(|| f.display().to_string())?
                .to_rgb8();
            Ok(Rgb {
                width: img.width() as usize,
                height: img.height() as usize,
                data: img.into_raw(),
            })
        })
        .collect()
}

/// Every k-th page of the archives, `n` of them, decoded to BGR (and the mean decode
/// time per page, one thread).
fn load_pages(archives: &[String], n: usize) -> Result<(Vec<bunko_ocr::image::BgrImage>, f64)> {
    let mut raw = Vec::new();
    for a in archives {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(a).with_context(|| a.clone())?)?;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            let name = f.name()?.to_string();
            let l = name.to_ascii_lowercase();
            if [".jpg", ".jpeg", ".png", ".webp"]
                .iter()
                .any(|e| l.ends_with(e))
            {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut bytes)?;
                files.push((name, bytes));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        raw.extend(files.into_iter().map(|(_, b)| b));
    }
    let step = (raw.len() / n.max(1)).max(1);
    let t0 = Instant::now();
    let pages = raw
        .iter()
        .step_by(step)
        .take(n)
        .map(|b| bunko_ocr::image::decode_bgr(b).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    let decode = t0.elapsed().as_secs_f64() / pages.len().max(1) as f64;
    Ok((pages, decode))
}

fn host_line() {
    let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "# host: {threads} hardware threads, loadavg {}",
        load.split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Differences from the reference run: (outputs whose text differs, outputs whose
/// detail differs).
fn diff(reference: &[(String, String)], out: &[(String, String)]) -> (usize, usize) {
    let text = reference
        .iter()
        .zip(out)
        .filter(|(a, b)| a.0 != b.0)
        .count();
    let detail = reference
        .iter()
        .zip(out)
        .filter(|(a, b)| a.1 != b.1)
        .count();
    (text, detail)
}

fn textbox(
    kind: &str,
    pos: &[String],
    opt: &std::collections::HashMap<String, String>,
) -> Result<()> {
    if pos.len() < 3 {
        bail!("usage: textbox_bench {kind} <model.onnx> <vocabulary.json> <crops-dir> [options]");
    }
    let (model, vocab) = (PathBuf::from(&pos[0]), PathBuf::from(&pos[1]));
    let n: usize = opt.get("n").map_or(Ok(600), |s| s.parse())?;
    let warm: usize = opt.get("warm").map_or(Ok(32), |s| s.parse())?;
    let threads = nums(opt.get("threads"), "1,2,4,8")?;
    let workers = nums(opt.get("workers"), "1,2,4,8,12,16")?;
    let crops = load_crops(Path::new(&pos[2]), n)?;
    let pixels: usize = crops.iter().map(|c| c.width * c.height).sum();
    host_line();
    println!(
        "# {kind}: {} crops, mean {:.0} px, warm-up {warm}",
        crops.len(),
        pixels as f64 / crops.len() as f64
    );
    println!("model\tT\tW\tcrops/s\twall_s\tcpu_s/crop\tcores_busy\ttext_diff\tdetail_diff");
    let mut reference: Option<Vec<(String, String)>> = None;
    for &t in &threads {
        let (nar, charspot) = if kind == "nar" {
            let weights = opt
                .get("weights")
                .context("nar needs --weights <model.safetensors> (the r16 graph holds none)")?;
            (
                Some(NarSmall::load(
                    &model,
                    Path::new(weights),
                    &vocab,
                    &opts(t),
                )?),
                None,
            )
        } else {
            (None, Some(Charspot::load(&model, &vocab, &opts(t))?))
        };
        let read = |i: usize| -> (String, String) {
            if let Some(m) = &nar {
                (m.read(&crops[i]).expect("nar-small read"), String::new())
            } else {
                let r = charspot
                    .as_ref()
                    .unwrap()
                    .read(&crops[i])
                    .expect("charspot read");
                let detail = format!(
                    "{:?}",
                    r.chars
                        .iter()
                        .map(|c| (c.bbox, c.score))
                        .collect::<Vec<_>>()
                );
                (r.text(), detail)
            }
        };
        run_pool(
            warm.min(crops.len()),
            *workers.iter().max().unwrap_or(&1),
            &read,
        );
        for &w in &workers {
            let r = timed(crops.len(), w, &read);
            let (dt, dd) = match &reference {
                Some(rf) => diff(rf, &r.out),
                None => (0, 0),
            };
            println!(
                "{kind}\t{t}\t{w}\t{:.1}\t{:.2}\t{:.4}\t{:.1}\t{dt}\t{dd}",
                crops.len() as f64 / r.wall,
                r.wall,
                r.cpu / crops.len() as f64,
                r.cpu / r.wall,
            );
            if reference.is_none() {
                reference = Some(r.out);
            }
        }
    }
    Ok(())
}

#[cfg(feature = "gpl-models")]
fn detector_threads(rule: &str, workers: usize) -> Result<usize> {
    Ok(if rule == "shipped" {
        bunko_engines::plan::SESSION_THREADS
            + bunko_engines::plan::SESSION_THREADS_PER_WORKER * (workers.max(1) - 1)
    } else if let Some(k) = rule.strip_prefix("per:") {
        k.parse::<usize>()? * workers
    } else {
        rule.parse()?
    })
}

#[cfg(feature = "gpl-models")]
fn animetext(pos: &[String], opt: &std::collections::HashMap<String, String>) -> Result<()> {
    use bunko_ocr::blocks::BlockDetector;
    if pos.len() < 2 {
        bail!("usage: textbox_bench animetext <yolo12x.onnx> <volume.cbz>... [options]");
    }
    let n: usize = opt.get("n").map_or(Ok(60), |s| s.parse())?;
    let warm: usize = opt.get("warm").map_or(Ok(4), |s| s.parse())?;
    let rules = list(opt.get("threads"), "shipped");
    let workers = nums(opt.get("workers"), "1,2,4,8")?;
    let (pages, decode) = load_pages(&pos[1..], n)?;
    host_line();
    println!(
        "# animetext: {} pages, decode {:.1} ms/page on one thread (not in the timings)",
        pages.len(),
        decode * 1e3
    );
    println!("model\trule\tT\tW\tpages/s\twall_s\tcpu_s/page\tcores_busy\tblocks/page\tdiff");
    let mut reference: Option<Vec<(String, String)>> = None;
    for rule in &rules {
        for &w in &workers {
            let t = detector_threads(rule, w)?;
            let det = bunko_ocr::animetext::AnimeText::load(Path::new(&pos[0]), &opts(t))?;
            let read = |i: usize| -> (String, String) {
                let (blocks, _) = det.detect(&pages[i]).expect("animetext");
                (
                    blocks.len().to_string(),
                    format!(
                        "{:?}",
                        blocks.iter().map(|b| (b.bbox, b.score)).collect::<Vec<_>>()
                    ),
                )
            };
            run_pool(warm.max(w).min(pages.len()), w, &read);
            let r = timed(pages.len(), w, &read);
            let blocks: usize = r
                .out
                .iter()
                .map(|o| o.0.parse::<usize>().unwrap_or(0))
                .sum();
            let d = reference.as_ref().map_or(0, |rf| diff(rf, &r.out).1);
            println!(
                "animetext\t{rule}\t{t}\t{w}\t{:.2}\t{:.2}\t{:.3}\t{:.1}\t{:.1}\t{d}",
                pages.len() as f64 / r.wall,
                r.wall,
                r.cpu / pages.len() as f64,
                r.cpu / r.wall,
                blocks as f64 / pages.len() as f64,
            );
            if reference.is_none() {
                reference = Some(r.out);
            }
        }
    }
    Ok(())
}

/// `W:T`.
#[cfg(feature = "gpl-models")]
fn pair(s: Option<&String>) -> Result<(usize, usize)> {
    let s = s.context("--engine and --detect take W:T")?;
    let (w, t) = s.split_once(':').context("W:T")?;
    Ok((w.parse()?, t.parse()?))
}

#[cfg(feature = "gpl-models")]
fn mixed(pos: &[String], opt: &std::collections::HashMap<String, String>) -> Result<()> {
    use bunko_ocr::blocks::BlockDetector;
    if pos.len() < 5 {
        bail!(
            "usage: textbox_bench mixed <nar.onnx> <vocabulary.json> <crops-dir> <yolo12x.onnx> <volume.cbz>... --engine W:T --detect W:T"
        );
    }
    let (ew, et) = pair(opt.get("engine"))?;
    let (dw, dt) = pair(opt.get("detect"))?;
    let secs: f64 = opt.get("secs").map_or(Ok(20.0), |s| s.parse())?;
    let crops = load_crops(
        Path::new(&pos[2]),
        opt.get("n").map_or(Ok(600), |s| s.parse())?,
    )?;
    let (pages, _) = load_pages(&pos[4..], 60)?;
    let weights = opt
        .get("weights")
        .context("mixed needs --weights <model.safetensors> (the r16 graph holds none)")?;
    let nar = NarSmall::load(
        Path::new(&pos[0]),
        Path::new(weights),
        Path::new(&pos[1]),
        &opts(et),
    )?;
    let det = bunko_ocr::animetext::AnimeText::load(Path::new(&pos[3]), &opts(dt))?;
    for c in crops.iter().take(16) {
        nar.read(c)?;
    }
    for p in pages.iter().take(2) {
        det.detect(p)?;
    }
    host_line();
    let stop = AtomicBool::new(false);
    let (crops_done, pages_done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (next_crop, next_page) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (t0, c0) = (Instant::now(), cpu_secs());
    std::thread::scope(|s| {
        for _ in 0..ew {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let i = next_crop.fetch_add(1, Ordering::Relaxed) % crops.len();
                    nar.read(&crops[i]).expect("nar-small read");
                    crops_done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        for _ in 0..dw {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let i = next_page.fetch_add(1, Ordering::Relaxed) % pages.len();
                    det.detect(&pages[i]).expect("animetext");
                    pages_done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    let (wall, cpu) = (t0.elapsed().as_secs_f64(), cpu_secs() - c0);
    let (c, p) = (
        crops_done.load(Ordering::Relaxed) as f64,
        pages_done.load(Ordering::Relaxed) as f64,
    );
    println!("engine W:T\tdetect W:T\tcrops/s\tpages/s\tcores_busy");
    println!(
        "{ew}:{et}\t{dw}:{dt}\t{:.1}\t{:.2}\t{:.1}",
        c / wall,
        p / wall,
        cpu / wall
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(kind) = args.first() else {
        bail!("usage: textbox_bench <nar|charspot|animetext|mixed> ...");
    };
    let (pos, opt) = split_args(&args[1..]);
    match kind.as_str() {
        "nar" | "charspot" => textbox(kind, &pos, &opt),
        #[cfg(feature = "gpl-models")]
        "animetext" => animetext(&pos, &opt),
        #[cfg(feature = "gpl-models")]
        "mixed" => mixed(&pos, &opt),
        _ => bail!("unknown mode {kind} (animetext and mixed need the gpl-models feature)"),
    }
}
