//! The public catalog's "Popular" order (design §10, "Catalog: Popular"): series ranked
//! by whole downloads over the last [`POPULAR_DAYS`] UTC days, every account's and
//! anonymous visitor's completed downloads counted, processors' never.
//!
//! The counts live in memory, an `Arc` swapped whole, computed off the request path
//! from `analytics.sqlite`: at start, after a flush that wrote downloads, when the UTC
//! day turns and with the hourly upkeep. `/catalog/api/library` reads the snapshot and
//! never queries the analytics database; it hands out only each listed series' rank
//! ([`ranks`]), never a count.

use crate::analytics::classify::series_key;
use bunko_db::analytics::AnalyticsDb;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// The days a download counts toward the order: today and the 29 before it (UTC).
pub const POPULAR_DAYS: i64 = 30;

/// Whole downloads per series key (folded), the latest computed.
#[derive(Clone, Default)]
pub struct Popularity(Arc<RwLock<Snapshot>>);

#[derive(Clone, Default)]
struct Snapshot {
    counts: Arc<HashMap<String, u64>>,
    /// The UTC day it was computed for (`None`: never).
    day: Option<i64>,
}

impl Popularity {
    /// The counts now (a cheap clone of the `Arc`).
    pub fn counts(&self) -> Arc<HashMap<String, u64>> {
        self.0.read().counts.clone()
    }

    /// The UTC day the counts were computed for (`None`: not yet).
    pub fn day(&self) -> Option<i64> {
        self.0.read().day
    }

    /// Compute the counts as of `now_s` (epoch seconds) and swap them in. Blocking:
    /// one indexed query over `volume_downloads`.
    pub fn refresh(&self, db: &AnalyticsDb, now_s: i64) -> bunko_db::Result<()> {
        let today = now_s.div_euclid(86400);
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (key, n) in db.series_download_counts(today - (POPULAR_DAYS - 1), today)? {
            // Stored keys are folded already; folding again keeps an older row's
            // spelling with today's catalog.
            *counts.entry(series_key(&key)).or_default() += n;
        }
        *self.0.write() = Snapshot {
            counts: Arc::new(counts),
            day: Some(today),
        };
        Ok(())
    }
}

/// The rank of each of `series` (folder names, as a catalog listing shows them) by
/// `counts`: 1 = the most downloaded, series with equal counts share a rank (the
/// catalog orders those by title), a series with no download has none. Ranks count
/// only the series given, so they reveal nothing about series the listing leaves out.
pub fn ranks(counts: &HashMap<String, u64>, series: &[&str]) -> Vec<Option<u32>> {
    let found: Vec<u64> = series
        .iter()
        .map(|name| counts.get(&series_key(name)).copied().unwrap_or(0))
        .collect();
    let mut distinct: Vec<u64> = found.iter().copied().filter(|n| *n > 0).collect();
    distinct.sort_unstable_by(|a, b| b.cmp(a));
    distinct.dedup();
    found
        .iter()
        .map(|n| {
            if *n == 0 {
                return None;
            }
            distinct
                .binary_search_by(|d| n.cmp(d))
                .ok()
                .and_then(|i| u32::try_from(i + 1).ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_are_dense_over_the_listed_series_only() {
        let counts: HashMap<String, u64> = [
            ("alpha saga".to_string(), 5),
            ("beta days".to_string(), 2),
            ("gamma run".to_string(), 5),
            ("hidden".to_string(), 9),
            ("delta".to_string(), 1),
        ]
        .into_iter()
        .collect();
        let got = ranks(
            &counts,
            &["Beta Days", "ALPHA  Saga", "Gamma Run", "Epsilon", "delta"],
        );
        // "hidden" is not listed: the top listed series is still 1.
        assert_eq!(got, [Some(2), Some(1), Some(1), None, Some(3)]);
        assert_eq!(ranks(&HashMap::new(), &["a", "b"]), [None, None]);
        assert!(ranks(&counts, &[]).is_empty());
    }

    /// The catalog lists a series under whatever spelling its folder has now; its
    /// downloads count under every spelling one host takes for that folder, rows
    /// written under an older key included.
    #[test]
    fn a_series_ranks_under_every_spelling_of_its_folder() {
        use bunko_db::analytics::traffic::DownloadRow;
        let db = AnalyticsDb::open_in_memory().unwrap();
        let day = 20_000i64;
        let row = |principal, series_key, series_title| DownloadRow {
            day,
            principal,
            volume_key: "v.cbz",
            series_key,
            series_title,
            bytes: 100,
            size: 100,
        };
        // Before the rename, under the catalog's key of the time; after it; another.
        let (old, new, other) = (
            bunko_db::fold_series_title_key("Straße"),
            series_key("STRASSE"),
            series_key("Other"),
        );
        db.write(|tx| {
            tx.add_download(&row("u:a", &old, "Straße"))?;
            tx.add_download(&row("u:b", &new, "STRASSE"))?;
            tx.add_download(&row("u:c", &other, "Other"))
        })
        .unwrap();
        let p = Popularity::default();
        p.refresh(&db, day * 86400 + 60).unwrap();
        let counts = p.counts();
        for listed in ["STRASSE", "Straße", "strasse"] {
            assert_eq!(
                ranks(&counts, &[listed, "Other"]),
                [Some(1), Some(2)],
                "{listed}"
            );
        }
    }
}
