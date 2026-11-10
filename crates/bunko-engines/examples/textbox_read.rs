//! Read image files with one of the text-box recognizers and print one JSON line per
//! file: `{"file", "text", "chars": [{"text", "bbox", "score"}]}` (`chars` for charspot
//! only). For comparing with the models' own Python packages.
//!
//! ```text
//! cargo run -p bunko-engines --release --features nc-models --example textbox_read -- \
//!     <charspot|nar> <model.onnx> <vocabulary.json> <image>...
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bunko_engines::textbox::{Charspot, NarSmall};
use bunko_ocr::runtime::RuntimeOptions;
use bunko_vlm::Rgb;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        bail!("usage: textbox_read <charspot|nar> <model.onnx> <vocabulary.json> <image>...");
    }
    let (model, vocab) = (PathBuf::from(&args[1]), PathBuf::from(&args[2]));
    let opts = RuntimeOptions {
        intra_threads: 8,
        ..Default::default()
    };
    let charspot = (args[0] == "charspot")
        .then(|| Charspot::load(&model, &vocab, &opts))
        .transpose()?;
    let nar = (args[0] == "nar")
        .then(|| NarSmall::load(&model, &vocab, &opts))
        .transpose()?;
    if charspot.is_none() && nar.is_none() {
        bail!("unknown recognizer {}", args[0]);
    }
    for file in &args[3..] {
        let img = image::open(file).with_context(|| file.clone())?.to_rgb8();
        let crop = Rgb {
            width: img.width() as usize,
            height: img.height() as usize,
            data: img.into_raw(),
        };
        let line = if let Some(c) = &charspot {
            let read = c.read(&crop)?;
            serde_json::json!({
                "file": file,
                "text": read.text(),
                "chars": read.chars.iter().map(|c| serde_json::json!({
                    "text": c.text, "bbox": c.bbox, "score": c.score,
                })).collect::<Vec<_>>(),
            })
        } else {
            serde_json::json!({"file": file, "text": nar.as_ref().unwrap().read(&crop)?})
        };
        println!("{line}");
    }
    Ok(())
}
