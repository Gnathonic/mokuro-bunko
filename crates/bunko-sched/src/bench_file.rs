//! `<storage>/.ocr-bench.json`: `{gen_id: {best: {pages_per_second, …},
//! baseline: {…}, startup_seconds, …}}` (`BenchService._load` / `_save` /
//! `prune`). The rate model reads it as a prior; the benchmark service
//! writes one finished result per row.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::SchedError;
use crate::pyjson::{dumps_indent2, read_object, write_atomic};
use crate::rate::BENCH_FILE;

#[derive(Clone, Debug)]
pub struct BenchFile {
    pub path: PathBuf,
}

impl BenchFile {
    pub fn new(storage: &Path) -> Self {
        BenchFile {
            path: storage.join(BENCH_FILE),
        }
    }

    /// Every saved result (object values only), file order.
    pub fn load(&self) -> Map<String, Value> {
        read_object(&self.path)
            .map(|m| m.into_iter().filter(|(_, v)| v.is_object()).collect())
            .unwrap_or_default()
    }

    /// `_save`: keep one finished result, pruned to `known_ids` (+ this row).
    pub fn save(
        &self,
        generation_id: &str,
        result: Map<String, Value>,
        known_ids: &[String],
    ) -> Result<(), SchedError> {
        let mut history = self.load();
        history.insert(generation_id.to_owned(), Value::Object(result));
        history.retain(|k, _| k == generation_id || known_ids.iter().any(|id| id == k));
        write_atomic(&self.path, &dumps_indent2(&Value::Object(history))).map_err(|e| {
            SchedError::Io {
                path: self.path.clone(),
                source: e,
            }
        })
    }

    /// `prune`: drop rows that no longer exist; delete the file when empty.
    /// Writes only when something was dropped.
    pub fn prune(&self, known_ids: &[String]) -> Result<(), SchedError> {
        let history = self.load();
        let before = history.len();
        let pruned: Map<String, Value> = history
            .into_iter()
            .filter(|(k, _)| known_ids.iter().any(|id| id == k))
            .collect();
        if pruned.len() == before {
            return Ok(());
        }
        if pruned.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SchedError::Io {
                    path: self.path.clone(),
                    source: e,
                }),
            };
        }
        write_atomic(&self.path, &dumps_indent2(&Value::Object(pruned))).map_err(|e| {
            SchedError::Io {
                path: self.path.clone(),
                source: e,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn save_prunes_and_prune_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let file = BenchFile::new(dir.path());
        let result = |pps: f64| {
            json!({"best": {"pages_per_second": pps}})
                .as_object()
                .cloned()
                .unwrap()
        };
        file.save("g-1", result(2.0), &["g-1".into(), "g-2".into()])
            .unwrap();
        file.save("g-2", result(3.0), &["g-2".into()]).unwrap();
        assert_eq!(file.load().keys().collect::<Vec<_>>(), vec!["g-2"]);
        let text = std::fs::read_to_string(&file.path).unwrap();
        assert_eq!(
            text,
            "{\n  \"g-2\": {\n    \"best\": {\n      \"pages_per_second\": 3.0\n    }\n  }\n}"
        );
        file.prune(&[]).unwrap();
        assert!(!file.path.exists());
    }
}
