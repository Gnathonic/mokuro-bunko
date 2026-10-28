//! `interop/budgets.toml`: the maximum each metric may reach (docs/design/
//! interop-testing.md §8.3). A budget naming no measured metric is a breach, so a typo
//! cannot pass silently.

use super::Metrics;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub catalog: BTreeMap<String, f64>,
}

#[derive(Debug, PartialEq)]
pub struct Breach {
    pub metric: String,
    pub budget: f64,
    pub measured: Option<f64>,
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.measured {
            Some(value) => write!(f, "{}: {value:.2} > budget {}", self.metric, self.budget),
            None => write!(
                f,
                "{}: no such metric (budget {})",
                self.metric, self.budget
            ),
        }
    }
}

pub fn load(path: &Path) -> Result<Budgets> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn check(budgets: &BTreeMap<String, f64>, metrics: &Metrics) -> Vec<Breach> {
    budgets
        .iter()
        .filter_map(|(metric, &budget)| match metrics.get(metric) {
            Some(&value) if value <= budget => None,
            measured => Some(Breach {
                metric: metric.clone(),
                budget,
                measured: measured.copied(),
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(pairs: &[(&str, f64)]) -> Metrics {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn within_budget_passes_and_over_budget_breaches() {
        let budgets: Budgets = toml::from_str("[catalog]\na_ms = 5\nb_ms = 10.5\n").unwrap();
        let m = metrics(&[("a_ms", 5.0), ("b_ms", 11.0), ("unbudgeted", 99.0)]);
        assert_eq!(
            check(&budgets.catalog, &m),
            vec![Breach {
                metric: "b_ms".into(),
                budget: 10.5,
                measured: Some(11.0)
            }]
        );
    }

    #[test]
    fn a_budget_with_no_metric_is_a_breach() {
        let budgets: Budgets = toml::from_str("[catalog]\nlibary_warm_ms = 25\n").unwrap();
        let breaches = check(&budgets.catalog, &metrics(&[("library_warm_ms", 1.0)]));
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].measured, None);
    }

    #[test]
    fn the_committed_budgets_parse() {
        let path = crate::util::workspace_root().join("interop/budgets.toml");
        assert!(!load(&path).unwrap().catalog.is_empty());
    }
}
