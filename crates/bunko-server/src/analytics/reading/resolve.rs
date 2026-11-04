//! uuid → series (design §6): a reader names volumes by their `volume_uuid`; the
//! library knows each archive's from its primary sidecar (`volume_identities`) and
//! from the compiled series entries (a volume without a sidecar has the reader's
//! deterministic id there). The map of both is kept in memory and rebuilt at most every
//! [`REFRESH`]; an id it lacks is looked up in `volume_identities` (indexed) before the
//! caller falls back to the series the reader's own `volume-data.json` names.
//!
//! Everything here blocks (the main database): call it from `spawn_blocking`.

use bunko_db::Database;
use bunko_db::analytics::reading::Attribution;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How old the map may get before the next lookup rebuilds it.
pub const REFRESH: Duration = Duration::from_secs(600);
/// Most volumes the map holds (about 150 bytes each, so at most about 7.5 MiB); past
/// it, sidecar ids are looked up one by one in `volume_identities`.
pub const MAX_VOLUMES: usize = 50_000;

/// The archive extensions (`sidecar_names::ARCHIVE_EXTENSIONS`), any case.
const ARCHIVE_EXTENSIONS: [&str; 4] = [".cbz", ".cbr", ".zip", ".rar"];

struct UuidMap {
    built: Instant,
    by_uuid: HashMap<Box<str>, Box<str>>,
}

#[derive(Default)]
pub struct Resolver {
    map: Mutex<Option<Arc<UuidMap>>>,
}

/// `Series/Vol 1.cbz` → `Vol 1`.
pub fn volume_stem(volume_key: &str) -> &str {
    let name = volume_key.rsplit_once('/').map_or(volume_key, |(_, n)| n);
    for ext in ARCHIVE_EXTENSIONS {
        if name.len() > ext.len()
            && name.is_char_boundary(name.len() - ext.len())
            && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
        {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

/// The series folder of a library archive (its parent, as `classify` attributes a
/// download), folded; `None` for an archive at the library root.
pub fn series_of_key(volume_key: &str) -> Option<(String, String)> {
    let (parent, _) = volume_key.trim_matches('/').rsplit_once('/')?;
    let title = parent.trim_matches('/');
    (!title.is_empty()).then(|| (bunko_db::fold_series_title_key(title), title.to_string()))
}

/// A volume's identity within its series, the same for every spelling of it:
/// `fold(series folder / stem)`.
pub fn volume_fold(series_title: &str, stem: &str) -> String {
    bunko_db::fold_series_title_key(&format!("{}/{}", series_title.trim_matches('/'), stem))
}

/// Where a library archive's reading goes.
pub fn attribution_of_key(volume_key: &str) -> Attribution {
    let (series_key, series_title) = series_of_key(volume_key).unwrap_or_default();
    Attribution {
        volume_key: Some(volume_key.to_string()),
        series_key,
        series_title: (!series_title.is_empty()).then_some(series_title),
        volume_title: Some(volume_stem(volume_key).to_string()),
    }
}

impl Resolver {
    pub fn new() -> Resolver {
        Resolver::default()
    }

    /// Drop the map (the next lookup rebuilds it).
    pub fn invalidate(&self) {
        *self.map.lock() = None;
    }

    fn map(&self, db: &Database) -> Option<Arc<UuidMap>> {
        let mut slot = self.map.lock();
        if let Some(m) = slot.as_ref()
            && m.built.elapsed() < REFRESH
        {
            return Some(m.clone());
        }
        match db.analytics_volume_uuids(MAX_VOLUMES) {
            Ok(rows) => {
                let mut by_uuid: HashMap<Box<str>, Box<str>> =
                    HashMap::with_capacity(rows.len().min(MAX_VOLUMES));
                for (key, uuid) in rows {
                    // The first archive (by key) wins when copies share an id.
                    by_uuid
                        .entry(uuid.into_boxed_str())
                        .or_insert(key.into_boxed_str());
                }
                let m = Arc::new(UuidMap {
                    built: Instant::now(),
                    by_uuid,
                });
                *slot = Some(m.clone());
                Some(m)
            }
            Err(e) => {
                tracing::warn!("[ANALYTICS] the volume id map cannot be read: {e}");
                slot.clone()
            }
        }
    }

    /// The library archive the reader's `uuid` names.
    pub fn volume_key(&self, db: &Database, uuid: &str) -> Option<String> {
        if let Some(key) = self.map(db).as_ref().and_then(|m| m.by_uuid.get(uuid)) {
            return Some(key.to_string());
        }
        // A sidecar recorded since the map was built, or one past its bound (indexed).
        db.analytics_volume_key_of_uuid(uuid).ok().flatten()
    }

    /// The library's attribution of each of `uuids` it knows.
    pub fn attributions<'a>(
        &self,
        db: &Database,
        uuids: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, Attribution> {
        let mut out = HashMap::new();
        for uuid in uuids {
            if out.contains_key(uuid) {
                continue;
            }
            if let Some(key) = self.volume_key(db, uuid) {
                out.insert(uuid.to_string(), attribution_of_key(&key));
            }
        }
        out
    }

    /// `(uuid, volume_key)` of every mapped volume of one series.
    pub fn volumes_of_series(&self, db: &Database, series_key: &str) -> Vec<(String, String)> {
        let Some(map) = self.map(db) else {
            return Vec::new();
        };
        map.by_uuid
            .iter()
            .filter(|(_, key)| series_of_key(key).is_some_and(|(k, _)| k == series_key))
            .map(|(u, k)| (u.to_string(), k.to_string()))
            .collect()
    }

    /// `uuid → volume_key` of every mapped volume.
    pub fn all_volumes(&self, db: &Database) -> HashMap<String, String> {
        self.map(db)
            .map(|m| {
                m.by_uuid
                    .iter()
                    .map(|(u, k)| (u.to_string(), k.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_folds() {
        assert_eq!(volume_stem("Kingdom/Vol 1.CBZ"), "Vol 1");
        assert_eq!(volume_stem("A/B/v.zip"), "v");
        assert_eq!(volume_stem("A/notes.txt"), "notes.txt");
        assert_eq!(
            series_of_key("A/B/v.cbz"),
            Some(("a/b".to_string(), "A/B".to_string()))
        );
        assert_eq!(series_of_key("v.cbz"), None);
        // Case variants and whitespace are one series, one volume.
        assert_eq!(
            series_of_key("KINGDOM  Saga/v1.cbz").map(|s| s.0),
            series_of_key("kingdom saga/v1.cbz").map(|s| s.0)
        );
        assert_eq!(
            volume_fold("Kingdom", "Vol 1"),
            volume_fold("kingdom", "VOL 1")
        );
        let a = attribution_of_key("Kingdom/Vol 1.cbz");
        assert_eq!(a.series_key, "kingdom");
        assert_eq!(a.volume_title.as_deref(), Some("Vol 1"));
    }

    #[test]
    fn resolves_through_the_main_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("mokuro.db")).unwrap();
        db.remember_volume_uuid("Kingdom/v1.cbz", "uuid-1").unwrap();
        let r = Resolver::new();
        assert_eq!(
            r.volume_key(&db, "uuid-1").as_deref(),
            Some("Kingdom/v1.cbz")
        );
        // Recorded after the map was built: found by the indexed lookup.
        db.remember_volume_uuid("kingdom/v2.cbz", "uuid-2").unwrap();
        let got = r.attributions(&db, ["uuid-2", "uuid-1", "nope"]);
        assert_eq!(got.len(), 2);
        assert_eq!(got["uuid-2"].series_key, "kingdom");
        r.invalidate();
        let mut vols = r.volumes_of_series(&db, "kingdom");
        vols.sort();
        assert_eq!(vols.len(), 2);
        assert_eq!(r.all_volumes(&db).len(), 2);
    }
}
