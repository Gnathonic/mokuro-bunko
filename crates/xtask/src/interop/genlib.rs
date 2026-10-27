//! A generated library for the catalog timings: `Series 0000`… with `volumes` archives
//! each, plus one `Small Series` of 3 volumes (the series view under test).

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

/// A small real JPEG, stored (not deflated) in every page slot.
const PAGE: &[u8] = include_bytes!("../../../../interop/fixtures/page.jpg");
const PAGES_PER_VOLUME: usize = 4;
pub const SMALL_SERIES: &str = "Small Series";
const SMALL_SERIES_VOLUMES: usize = 3;

pub struct Generated {
    #[cfg_attr(not(test), allow(dead_code))]
    pub library: PathBuf,
    pub series: Vec<String>,
    pub volumes: usize,
}

impl Generated {
    /// `(series, volume)` of the `n`-th volume, cycling through the library.
    pub fn volume(&self, n: usize, per_series: usize) -> (String, String) {
        let series = &self.series[n % self.series.len()];
        let count = if series == SMALL_SERIES {
            SMALL_SERIES_VOLUMES
        } else {
            per_series
        };
        let v = (n / self.series.len()) % count.max(1);
        (series.clone(), volume_name(series, v))
    }
}

fn volume_name(series: &str, v: usize) -> String {
    format!("{series} v{:02}", v + 1)
}

/// Write the library under `root/library` (replacing nothing: `root` must be empty).
pub fn generate(root: &Path, series: usize, volumes: usize) -> Result<Generated> {
    let library = root.join("library");
    std::fs::create_dir_all(&library)?;
    let mut names: Vec<String> = (0..series).map(|s| format!("Series {s:04}")).collect();
    names.push(SMALL_SERIES.to_string());
    let mut total = 0;
    for name in &names {
        let dir = library.join(name);
        std::fs::create_dir_all(&dir)?;
        let count = if name == SMALL_SERIES {
            SMALL_SERIES_VOLUMES
        } else {
            volumes
        };
        for v in 0..count {
            let title = volume_name(name, v);
            write_cbz(&dir.join(format!("{title}.cbz")))?;
            if total % 2 == 0 {
                write_mokuro(&dir.join(format!("{title}.mokuro")), name, &title, total)?;
            }
            total += 1;
        }
    }
    Ok(Generated {
        library,
        series: names,
        volumes: total,
    })
}

fn write_cbz(path: &Path) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for p in 0..PAGES_PER_VOLUME {
        zip.start_file(format!("{p:03}.jpg"), options)?;
        zip.write_all(PAGE)?;
    }
    zip.finish()?.flush()?;
    Ok(())
}

/// The OCR sidecar as the reader and Bunko read it: one line of text per page.
pub fn mokuro_doc(series: &str, title: &str, n: usize) -> Vec<u8> {
    let pages: Vec<serde_json::Value> = (0..PAGES_PER_VOLUME)
        .map(|p| {
            serde_json::json!({
                "img_path": format!("{p:03}.jpg"),
                "img_width": 64,
                "img_height": 96,
                "blocks": [{"box": [1, 1, 10, 10], "vertical": true, "lines": ["日本語です"]}],
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.2",
        "title": series,
        "volume": title,
        "volume_uuid": format!("00000000-0000-4000-8000-{n:012}"),
        "pages": pages,
    }))
    .expect("static JSON")
}

fn write_mokuro(path: &Path, series: &str, title: &str, n: usize) -> Result<()> {
    std::fs::write(path, mokuro_doc(series, title, n))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_series_volumes_and_every_other_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let generated = generate(dir.path(), 3, 2).unwrap();
        assert_eq!(generated.series.len(), 4);
        assert_eq!(generated.volumes, 3 * 2 + 3);
        let mut cbz = 0;
        let mut mokuro = 0;
        for series in &generated.series {
            for entry in std::fs::read_dir(generated.library.join(series)).unwrap() {
                let path = entry.unwrap().path();
                match path.extension().and_then(|e| e.to_str()) {
                    Some("cbz") => {
                        cbz += 1;
                        let archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap());
                        assert_eq!(archive.unwrap().len(), PAGES_PER_VOLUME);
                    }
                    Some("mokuro") => mokuro += 1,
                    other => panic!("unexpected file {other:?}"),
                }
            }
        }
        assert_eq!((cbz, mokuro), (9, 5));
        assert_eq!(
            generated.volume(3, 2),
            (SMALL_SERIES.to_string(), "Small Series v01".to_string())
        );
    }
}
