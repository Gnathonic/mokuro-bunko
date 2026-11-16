//! The file-type walk (design §7): one folder at a time, stat only (on a cloud library
//! every listing and size comes from the store's index; nothing is opened, nothing is
//! downloaded), folded into bounded totals. Series are keyed by
//! `fold_series_title_key` of the folder's library-relative path, so every spelling of
//! one series (case, Unicode form, whitespace) is one series.

use bunko_core::names;
use bunko_core::reader_tree;
use bunko_core::sidecar_names::{self, SidecarClass};
use bunko_db::analytics::AnalyticsDb;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The kinds a file is counted as, in display order, with their labels.
pub const KINDS: [(&str, &str); 8] = [
    ("archive", "Archives"),
    ("primary_sidecar", "OCR sidecars"),
    ("layer_sidecar", "OCR layers"),
    ("cover", "Covers"),
    ("nocover", "No-cover markers"),
    ("series_json", "series.json"),
    ("catalog_json", "catalog.json"),
    ("other", "Other files"),
];
const ARCHIVE: usize = 0;
const PRIMARY: usize = 1;
const LAYER: usize = 2;
const COVER: usize = 3;
const NOCOVER: usize = 4;
const SERIES_JSON: usize = 5;
const CATALOG_JSON: usize = 6;
const OTHER: usize = 7;

/// The archive extensions, in mix order (`cbz+zip`).
const ARCHIVE_EXTS: [&str; 4] = ["cbz", "cbr", "zip", "rar"];

/// Distinct extensions kept; the rest are summed as `(other)`. Extensions are chosen by
/// whoever uploads files, so the map is capped.
pub const MAX_EXTS: usize = 64;
/// The longest extension kept as itself.
const MAX_EXT_LEN: usize = 12;
/// Series listed by name in `no_sidecar`.
pub const LIST_LIMIT: usize = 200;
/// Archives one pass queues for page reading at most (the rest wait for the next
/// pass, which then starts at once): the queue is the walk's only per-archive memory.
pub const QUEUE_MAX: usize = 20_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub files: u64,
    pub bytes: u64,
}

impl Count {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

/// One series (every folder spelling of it) as the walk saw it.
#[derive(Debug, Clone, Default)]
pub struct SeriesAcc {
    /// The first spelling met.
    pub title: String,
    pub volumes: u64,
    pub bytes: u64,
    /// Bit `i`: holds an [`ARCHIVE_EXTS`]`[i]` archive.
    pub mix: u8,
    pub sidecars: u64,
}

/// An archive whose page formats are to be read (local libraries only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub volume_key: String,
    pub path: PathBuf,
}

/// The walk's running totals.
pub struct Walk {
    pub kinds: [Count; 8],
    pub exts: BTreeMap<String, Count>,
    pub series: HashMap<String, SeriesAcc>,
    /// Archives directly in the library root (no series).
    pub loose_archives: u64,
    /// Folders that could not be listed.
    pub unlisted: u64,
    /// Archives to read for page formats, at most [`QUEUE_MAX`].
    pub queue: Vec<Queued>,
    /// Archives whose page formats are missing or stale (queued or not).
    pub pages_pending: u64,
    /// Hashes of every archive key seen (to drop the `archive_pages` rows of archives
    /// that are gone): 8 bytes an archive.
    pub seen: HashSet<u64>,
    /// Read page formats (a local library with `page_formats` on).
    pages: bool,
}

/// The hash [`Walk::seen`] keeps of a volume key.
pub fn key_hash(key: &str) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// The lowercase extension `by_ext` counts a file under (`""`: none).
pub fn ext_of(name: &str) -> String {
    if sidecar_names::strip_suffix_ci(name, ".mokuro.gz").is_some() {
        return "mokuro.gz".into();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => {
            let ext = &name[i + 1..];
            if ext.is_empty()
                || ext.len() > MAX_EXT_LEN
                || !ext.chars().all(|c| c.is_ascii_alphanumeric())
            {
                "(other)".into()
            } else {
                ext.to_ascii_lowercase()
            }
        }
        _ => String::new(),
    }
}

fn archive_index(name: &str) -> Option<usize> {
    ARCHIVE_EXTS
        .iter()
        .position(|e| sidecar_names::strip_suffix_ci(name, &format!(".{e}")).is_some())
}

/// Modification time as epoch nanoseconds.
pub fn mtime_ns(meta: &bunko_store::Meta) -> i64 {
    if let Some((s, ns)) = meta.unix_mtime() {
        return s.saturating_mul(1_000_000_000).saturating_add(ns);
    }
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

impl Walk {
    pub fn new(pages: bool) -> Walk {
        Walk {
            kinds: [Count::default(); 8],
            exts: BTreeMap::new(),
            series: HashMap::new(),
            loose_archives: 0,
            unlisted: 0,
            queue: Vec::new(),
            pages_pending: 0,
            seen: HashSet::new(),
            pages,
        }
    }

    fn count_ext(&mut self, name: &str, bytes: u64) {
        let ext = ext_of(name);
        if let Some(c) = self.exts.get_mut(&ext) {
            c.add(bytes);
        } else if self.exts.len() < MAX_EXTS {
            self.exts.entry(ext).or_default().add(bytes);
        } else {
            self.exts.entry("(other)".into()).or_default().add(bytes);
        }
    }

    /// Count the files of `dir` (library-relative `rel`, `""` for the root) and return
    /// the sub-folders to walk: hidden and symlinked folders are skipped, as the library
    /// index skips them, and so is the cloud-root owner's per-user tree (`history/`,
    /// `volume-data.json`, ..., in any spelling), which are no library files. A
    /// `thumbnails/` folder is an ordinary series folder, as it is everywhere else.
    /// `db` answers which archives already have current page formats.
    pub fn folder(
        &mut self,
        dir: &Path,
        rel: &str,
        db: Option<&AnalyticsDb>,
    ) -> Vec<(PathBuf, String)> {
        let Ok(entries) = bunko_store::fs::read_dir(dir) else {
            self.unlisted += 1;
            return Vec::new();
        };
        let root = rel.is_empty();
        let mut subdirs = Vec::new();
        // (name, size, mtime ns)
        let mut files: Vec<(String, u64, i64)> = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if root && reader_tree::is_per_user_root_name(&name) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if !name.starts_with('.') {
                    let child = if root {
                        name.clone()
                    } else {
                        format!("{rel}/{name}")
                    };
                    subdirs.push((entry.path(), child));
                }
                continue;
            }
            let meta = if ft.is_symlink() {
                // A symlinked folder is listed by nobody; a symlinked file counts as
                // what it points at.
                match bunko_store::fs::metadata(entry.path()) {
                    Ok(m) if m.is_file() => m,
                    _ => continue,
                }
            } else {
                match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                }
            };
            files.push((name, meta.len(), mtime_ns(&meta)));
        }
        subdirs.sort_by(|a, b| a.1.cmp(&b.1));
        if files.is_empty() {
            return subdirs;
        }
        let stems = sidecar_names::volume_stems(files.iter().map(|f| f.0.as_str()));
        let folder_key = (!root).then(|| crate::analytics::classify::series_key(rel));
        let mut acc = SeriesAcc {
            title: rel.to_string(),
            ..SeriesAcc::default()
        };
        let mut archives: Vec<(String, u64, i64)> = Vec::new();
        for (name, size, mtime) in &files {
            let kind = if let Some(i) = archive_index(name) {
                acc.mix |= 1 << i;
                acc.volumes += 1;
                archives.push((name.clone(), *size, *mtime));
                ARCHIVE
            } else {
                match sidecar_names::classify(name, &stems) {
                    SidecarClass::Primary { .. } => PRIMARY,
                    SidecarClass::Layer { .. } => LAYER,
                    SidecarClass::Orphan => {
                        if let Some(stem) = sidecar_names::strip_suffix_ci(name, ".webp")
                            && stems.contains(&sidecar_names::fold(stem))
                        {
                            COVER
                        } else if sidecar_names::strip_suffix_ci(name, ".nocover").is_some() {
                            NOCOVER
                        } else if !root && names::is_fixed_name(name, "series.json") {
                            SERIES_JSON
                        } else if root && names::is_fixed_name(name, "catalog.json") {
                            CATALOG_JSON
                        } else {
                            OTHER
                        }
                    }
                }
            };
            if matches!(kind, PRIMARY | LAYER) {
                acc.sidecars += 1;
            }
            self.kinds[kind].add(*size);
            self.count_ext(name, *size);
            acc.bytes += size;
        }
        if root {
            self.loose_archives += archives.len() as u64;
        }
        self.queue_archives(dir, rel, &archives, db);
        if let Some(key) = folder_key
            && acc.volumes > 0
        {
            match self.series.get_mut(&key) {
                Some(s) => {
                    s.volumes += acc.volumes;
                    s.bytes += acc.bytes;
                    s.mix |= acc.mix;
                    s.sidecars += acc.sidecars;
                }
                None => {
                    self.series.insert(key, acc);
                }
            }
        }
        subdirs
    }

    /// Note the folder's archives, and queue the ones whose page formats are missing or
    /// were read from another size or mtime.
    fn queue_archives(
        &mut self,
        dir: &Path,
        rel: &str,
        archives: &[(String, u64, i64)],
        db: Option<&AnalyticsDb>,
    ) {
        if !self.pages || archives.is_empty() {
            return;
        }
        let keys: Vec<String> = archives
            .iter()
            .map(|(name, _, _)| {
                if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                }
            })
            .collect();
        for k in &keys {
            self.seen.insert(key_hash(k));
        }
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let stamps = match db.map(|db| db.archive_stamps(&refs)) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                tracing::debug!("[ANALYTICS] library scan: page-format stamps unread: {e}");
                vec![None; keys.len()]
            }
            None => vec![None; keys.len()],
        };
        for ((key, (name, size, mtime)), stamp) in keys.into_iter().zip(archives).zip(stamps) {
            if stamp == Some((*size, *mtime)) {
                continue;
            }
            self.pages_pending += 1;
            if self.queue.len() < QUEUE_MAX {
                self.queue.push(Queued {
                    volume_key: key,
                    path: dir.join(name),
                });
            }
        }
    }

    pub fn total(&self) -> Count {
        let mut t = Count::default();
        for k in &self.kinds {
            t.files += k.files;
            t.bytes += k.bytes;
        }
        t
    }

    /// Volumes in series folders.
    pub fn volumes(&self) -> u64 {
        self.series.values().map(|s| s.volumes).sum()
    }

    /// The scan JSON stored in `library_scan` (what the API serves of it).
    pub fn summary(&self) -> Value {
        let total = self.total();
        let by_kind: Vec<Value> = KINDS
            .iter()
            .zip(&self.kinds)
            .filter(|(_, c)| c.files > 0)
            .map(|((kind, label), c)| json!({"kind": kind, "label": label, "files": c.files, "bytes": c.bytes}))
            .collect();
        let mut by_ext: Vec<(&String, &Count)> = self.exts.iter().collect();
        by_ext.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(b.0)));
        let by_ext: Vec<Value> = by_ext
            .into_iter()
            .map(|(ext, c)| json!({"ext": ext, "files": c.files, "bytes": c.bytes}))
            .collect();
        let mut mixes: BTreeMap<String, u64> = BTreeMap::new();
        let mut bare: Vec<(&String, &SeriesAcc)> = Vec::new();
        for (key, s) in &self.series {
            let mix: Vec<&str> = ARCHIVE_EXTS
                .iter()
                .enumerate()
                .filter(|(i, _)| s.mix & (1 << i) != 0)
                .map(|(_, e)| *e)
                .collect();
            *mixes.entry(mix.join("+")).or_default() += 1;
            if s.sidecars == 0 {
                bare.push((key, s));
            }
        }
        let mut archive_mix: Vec<(String, u64)> = mixes.into_iter().collect();
        archive_mix.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        bare.sort_by(|a, b| a.1.title.cmp(&b.1.title));
        let bare_count = bare.len();
        let bare: Vec<Value> = bare
            .into_iter()
            .take(LIST_LIMIT)
            .map(|(key, s)| json!({"series_key": key, "title": s.title, "volumes": s.volumes}))
            .collect();
        json!({
            "totals": {
                "series": self.series.len(),
                "volumes": self.volumes(),
                "bytes": total.bytes,
                "files": total.files,
            },
            "by_kind": by_kind,
            "by_ext": by_ext,
            "archive_mix": archive_mix.into_iter().map(|(mix, series)| json!({"mix": mix, "series": series})).collect::<Vec<_>>(),
            "no_sidecar": {"count": bare_count, "series": bare},
            "loose_archives": self.loose_archives,
            "unlisted_folders": self.unlisted,
        })
    }

    /// `{kind: {files, bytes}}` for the day's growth snapshot.
    pub fn kinds_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for ((kind, _), c) in KINDS.iter().zip(&self.kinds) {
            m.insert(
                (*kind).to_string(),
                json!({"files": c.files, "bytes": c.bytes}),
            );
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;

    fn tree(files: &[(&str, usize)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, len) in files {
            let p = dir.path().join(rel);
            if rel.ends_with('/') {
                std::fs::create_dir_all(&p).unwrap();
                continue;
            }
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![b'x'; *len]).unwrap();
        }
        dir
    }

    fn walk(root: &Path, pages: bool) -> Walk {
        let mut w = Walk::new(pages);
        let mut stack = vec![(root.to_path_buf(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let subs = w.folder(&dir, &rel, None);
            stack.extend(subs.into_iter().rev());
        }
        w
    }

    #[test]
    fn extensions() {
        assert_eq!(ext_of("Vol 1.CBZ"), "cbz");
        assert_eq!(ext_of("v.mokuro.GZ"), "mokuro.gz");
        assert_eq!(ext_of("v.edit.mokuro"), "mokuro");
        assert_eq!(ext_of("README"), "");
        assert_eq!(ext_of(".DS_Store"), "");
        assert_eq!(ext_of("x.averyveryverylongext"), "(other)");
        assert_eq!(ext_of("x.tar-gz"), "(other)");
    }

    #[test]
    fn kinds_series_and_mixes() {
        let d = tree(&[
            ("Kingdom/Kingdom 01.cbz", 100),
            ("Kingdom/Kingdom 01.mokuro", 10),
            ("Kingdom/Kingdom 01.webp", 5),
            ("Kingdom/Kingdom 01.curated.mokuro.gz", 3),
            ("Kingdom/series.json", 2),
            // Another spelling of the same series is the same series.
            ("kingdom/Kingdom 02.zip", 200),
            ("kingdom/Kingdom 02.nocover", 0),
            ("Nested/Deep Series/v1.CBR", 50),
            ("Nested/Deep Series/notes.txt", 7),
            ("Nested/Deep Series/v1.pdf", 9),
            ("loose.cbz", 1),
            ("catalog.json", 4),
            (".hidden/x.cbz", 1000),
            // An ordinary folder, as every other library walk sees it.
            ("thumbnails/a.webp", 1000),
            ("history/dev/2026-01.events", 1000),
            ("volume-data.json", 1000),
            ("Empty/", 0),
        ]);
        let w = walk(d.path(), false);
        let s = w.summary();
        assert_eq!(
            s["totals"],
            json!({"series": 2, "volumes": 3, "bytes": 1391, "files": 13})
        );
        let kinds: BTreeMap<String, (u64, u64)> = s["by_kind"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                (
                    k["kind"].as_str().unwrap().to_string(),
                    (k["files"].as_u64().unwrap(), k["bytes"].as_u64().unwrap()),
                )
            })
            .collect();
        assert_eq!(kinds["archive"], (4, 351));
        assert_eq!(kinds["primary_sidecar"], (1, 10));
        assert_eq!(kinds["layer_sidecar"], (1, 3));
        assert_eq!(kinds["cover"], (1, 5));
        assert_eq!(kinds["nocover"], (1, 0));
        assert_eq!(kinds["series_json"], (1, 2));
        assert_eq!(kinds["catalog_json"], (1, 4));
        assert_eq!(kinds["other"], (3, 1016));
        assert_eq!(
            s["archive_mix"],
            json!([{"mix": "cbr", "series": 1}, {"mix": "cbz+zip", "series": 1}])
        );
        assert_eq!(
            s["no_sidecar"],
            json!({"count": 1, "series": [{"series_key": "nested/deep series", "title": "Nested/Deep Series", "volumes": 1}]})
        );
        assert_eq!(s["loose_archives"], 1);
        let k = &w.series["kingdom"];
        assert_eq!((k.volumes, k.bytes, k.sidecars), (2, 320, 2));
        assert!(w.queue.is_empty(), "page formats off");
    }

    #[test]
    fn a_thumbnails_folder_is_a_series_in_any_spelling() {
        // The library has no folder of Bunko's own: the catalog lists this series.
        let d = tree(&[("thumbnails/v1.cbz", 10), ("Thumbnails/v2.cbz", 20)]);
        let w = walk(d.path(), false);
        assert_eq!(
            w.summary()["totals"],
            json!({"series": 1, "volumes": 2, "bytes": 30, "files": 2})
        );
        assert_eq!(w.series["thumbnails"].volumes, 2);
    }

    #[test]
    fn fixed_names_by_the_filename_fold() {
        let d = tree(&[
            ("S/v1.cbz", 10),
            ("S/\u{17f}eries.json", 2),
            ("Catalog\u{200B}.json", 3),
        ]);
        let s = walk(d.path(), false).summary();
        let kind = |k: &str| {
            s["by_kind"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["kind"] == k)
                .map(|x| x["files"].as_u64().unwrap())
        };
        assert_eq!(kind("series_json"), Some(1));
        assert_eq!(kind("catalog_json"), Some(1));
    }

    #[test]
    fn archives_queue_for_pages_and_bounded_extensions() {
        let mut files: Vec<(String, usize)> =
            (0..100).map(|i| (format!("S/f{i}.e{i}"), 1)).collect();
        files.push(("S/a.cbz".into(), 3));
        files.push(("b.zip".into(), 3));
        let refs: Vec<(&str, usize)> = files.iter().map(|(n, l)| (n.as_str(), *l)).collect();
        let d = tree(&refs);
        let w = walk(d.path(), true);
        assert!(w.exts.len() <= MAX_EXTS + 1);
        assert!(w.exts["(other)"].files > 0);
        let mut keys: Vec<&str> = w.queue.iter().map(|q| q.volume_key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["S/a.cbz", "b.zip"]);
        assert_eq!(w.pages_pending, 2);
        assert!(w.seen.contains(&key_hash("S/a.cbz")));
    }
}
