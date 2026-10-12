//! Raw ONNX Runtime timing of the PP-OCR models (no pre/post-processing).
use std::path::PathBuf;
use std::time::Instant;

use bunko_ocr::runtime::{Model, RuntimeOptions};

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).expect("models dir"));
    let threads: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let opts = RuntimeOptions {
        intra_threads: threads,
        ..Default::default()
    };
    let det = Model::load(&dir.join("det/manga_det_v0.2.onnx"), &opts)?;
    let rec = Model::load(&dir.join("rec/manga_rec_v0.2.onnx"), &opts)?;
    for _ in 0..3 {
        let t = Instant::now();
        det.run_f32(&[1, 3, 1120, 704], vec![0.1; 3 * 1120 * 704])?;
        let a = t.elapsed().as_secs_f64();
        let t = Instant::now();
        rec.run_f32(&[16, 3, 48, 320], vec![0.1; 16 * 3 * 48 * 320])?;
        let b = t.elapsed().as_secs_f64();
        let t = Instant::now();
        rec.run_f32(&[1, 3, 48, 320], vec![0.1; 3 * 48 * 320])?;
        let c = t.elapsed().as_secs_f64();
        println!("det {a:.3}s  rec16x320 {b:.3}s  rec1x320 {c:.3}s");
    }
    Ok(())
}
