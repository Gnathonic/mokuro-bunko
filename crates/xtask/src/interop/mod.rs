//! `cargo xtask interop`: the cross-party test rig (docs/design/interop-testing.md).
//! I0 has one suite, `catalog-perf`: the catalog and series views on a large library,
//! checked against `interop/budgets.toml`.

mod budgets;
mod catalog;
mod genlib;
mod http;
mod server;

use crate::util;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Metric name → value (milliseconds, counts or ratios).
pub type Metrics = BTreeMap<String, f64>;

#[derive(clap::Args)]
pub struct InteropArgs {
    #[command(subcommand)]
    suite: Suite,
}

#[derive(clap::Subcommand)]
enum Suite {
    /// Time the catalog and series views on a generated 2,000-series library.
    CatalogPerf(CatalogPerfArgs),
}

#[derive(clap::Args)]
struct CatalogPerfArgs {
    /// The server binary (default: target/release/mokuro-bunko).
    #[arg(long)]
    bin: Option<PathBuf>,
    /// Also time the catalog page in Chromium (needs `npm ci` in interop/).
    #[arg(long)]
    browser: bool,
    /// A small library and short load window, for a local smoke run (no budgets).
    #[arg(long)]
    quick: bool,
    /// Runs per attempt; each metric's budget applies to the median.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Report only; do not fail on budgets.
    #[arg(long)]
    no_check: bool,
}

pub fn run(args: &InteropArgs) -> Result<()> {
    match &args.suite {
        Suite::CatalogPerf(a) => catalog_perf(a),
    }
}

/// The `p`-th percentile (nearest rank) of `values`; sorts them.
pub fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * values.len() as f64).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

fn medians(runs: &[Metrics]) -> Metrics {
    let mut keys: Vec<&String> = runs.iter().flat_map(|m| m.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            let mut values: Vec<f64> = runs.iter().filter_map(|m| m.get(key).copied()).collect();
            (key.clone(), percentile(&mut values, 50.0))
        })
        .collect()
}

fn catalog_perf(a: &CatalogPerfArgs) -> Result<()> {
    let root = util::workspace_root();
    let target = util::target_dir(&root);
    let bin = a.bin.clone().unwrap_or_else(|| {
        target
            .join("release")
            .join(format!("mokuro-bunko{}", std::env::consts::EXE_SUFFIX))
    });
    if !bin.exists() {
        bail!(
            "{} not found: build it with `cargo build --release -p mokuro-bunko --no-default-features`",
            bin.display()
        );
    }
    let out_dir = target.join("interop");
    let budgets = budgets::load(&root.join("interop/budgets.toml"))?;
    let shape = if a.quick {
        catalog::Shape::QUICK
    } else {
        catalog::Shape::FULL
    };
    let script = a.browser.then(|| root.join("interop/e2e/catalog-perf.mjs"));
    let mut rig = catalog::Rig::prepare(&bin, &out_dir.join("work"), shape, script)?;

    let check = !a.no_check && !a.quick;
    let mut attempts = Vec::new();
    for attempt in 1..=2 {
        let mut runs = Vec::new();
        for n in 1..=a.runs {
            let m = rig.run()?;
            eprintln!("attempt {attempt} run {n}: {}", summary(&m));
            runs.push(m);
        }
        let median = medians(&runs);
        let breaches = budgets::check(&budgets.catalog, &median);
        attempts.push(serde_json::json!({"runs": runs, "median": median}));
        report(&median, &budgets.catalog);
        write_results(&out_dir, &attempts)?;
        if !check || breaches.is_empty() {
            return Ok(());
        }
        for breach in &breaches {
            eprintln!("over budget: {breach}");
        }
        if attempt == 1 {
            eprintln!("retrying the whole scenario once");
        } else {
            bail!(
                "{} catalog budget(s) missed on two attempts",
                breaches.len()
            );
        }
    }
    Ok(())
}

fn summary(m: &Metrics) -> String {
    m.iter()
        .map(|(k, v)| format!("{k}={v:.2}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn report(median: &Metrics, budgets: &BTreeMap<String, f64>) {
    println!(
        "{:<34} {:>10} {:>10}",
        "metric (median)", "measured", "budget"
    );
    for (key, value) in median {
        let budget = budgets
            .get(key)
            .map_or_else(|| "-".to_string(), |b| format!("{b}"));
        let flag = match budgets.get(key) {
            Some(b) if value > b => "  OVER",
            _ => "",
        };
        println!("{key:<34} {value:>10.2} {budget:>10}{flag}");
    }
}

fn write_results(out_dir: &std::path::Path, attempts: &[serde_json::Value]) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let doc = serde_json::json!({
        "suite": "catalog-perf",
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "attempts": attempts,
    });
    std::fs::write(
        out_dir.join("catalog-perf.json"),
        serde_json::to_vec_pretty(&doc)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&mut v, 50.0), 3.0);
        assert_eq!(percentile(&mut v, 100.0), 5.0);
        assert_eq!(percentile(&mut v, 95.0), 5.0);
        assert_eq!(percentile(&mut v, 0.0), 1.0);
    }

    #[test]
    fn medians_take_each_metric_across_runs() {
        let runs: Vec<Metrics> = [1.0, 9.0, 2.0]
            .iter()
            .map(|v| [("a".to_string(), *v)].into_iter().collect())
            .collect();
        assert_eq!(medians(&runs)["a"], 2.0);
    }
}
