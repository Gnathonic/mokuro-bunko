//! Print the device catalog, then load a model on the given targets and report
//! where it landed: `cargo run -p bunko-ocr --example devices -- <model.onnx> webgpu cpu`.
use bunko_ocr::runtime::{ExecutionTarget, Model, RuntimeOptions, device_catalog, ep_compiled};

fn main() -> anyhow::Result<()> {
    println!("compiled: {:?}", ep_compiled());
    println!("{}", serde_json::to_string_pretty(&device_catalog())?);
    let mut args = std::env::args().skip(1);
    if let Some(model) = args.next() {
        let targets: Vec<ExecutionTarget> = args.map(|a| a.parse()).collect::<Result<_, _>>()?;
        let m = Model::load(
            model.as_ref(),
            &RuntimeOptions {
                targets,
                ..Default::default()
            },
        )?;
        let t = std::time::Instant::now();
        m.run_f32(&[1, 3, 1120, 704], vec![0.1; 3 * 1120 * 704])?;
        let first = t.elapsed();
        let t = std::time::Instant::now();
        m.run_f32(&[1, 3, 1120, 704], vec![0.1; 3 * 1120 * 704])?;
        println!(
            "on {} (fallback: {:?}); first run {first:?}, second {:?}",
            m.target(),
            m.fallback_reason(),
            t.elapsed()
        );
    }
    Ok(())
}
