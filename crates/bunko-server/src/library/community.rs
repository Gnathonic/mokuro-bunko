//! Community enrichment (0.5.2 `catalog/community.py`, spec metadata-catalog §10):
//! ratings, tags and genres from AniList (batched GraphQL, primary) and MAL via Jikan
//! (series linked only there), stored in `community_details` and shown only by
//! `/catalog/api/library`. Everything fails quietly; the next cycle retries.
//!
//! Improvements over 0.5.2 (spec Q7): the User-Agent names the project URL, and a 429
//! answer's `Retry-After` is honoured (one wait, capped, then one retry) instead of
//! counting as a failure.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bunko_db::{CommunityDetails, Database};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub const ANILIST_GRAPHQL_URL: &str = "https://graphql.anilist.co";
/// `{id}` is replaced by the MAL id.
pub const JIKAN_MANGA_URL: &str = "https://api.jikan.moe/v4/manga/{id}";
/// AniList tags below this relevance rank are noise.
pub const TAG_RANK_FLOOR: f64 = 40.0;
pub const TAG_LIMIT: usize = 10;
/// Ids per GraphQL batch (AniList's page maximum).
pub const ANILIST_BATCH_SIZE: usize = 50;
/// Details older than this are refetched.
pub const REFRESH_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Longest `Retry-After` honoured before giving the request up for this cycle.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

const ANILIST_QUERY: &str = "
query ($ids: [Int]) {
  Page(page: 1, perPage: 50) {
    media(id_in: $ids, type: MANGA) {
      id
      meanScore
      genres
      tags { name rank }
    }
  }
}
";

/// Endpoints and timings (0.5.2's constants by default; tests shorten them).
#[derive(Debug, Clone)]
pub struct CommunitySettings {
    pub anilist_url: String,
    /// Contains `{id}`.
    pub jikan_url: String,
    /// Pause after every AniList batch and every Jikan fetch.
    pub request_gap: Duration,
    /// Full sweep interval.
    pub poll: Duration,
    /// First full sweep after start (nudges are served at once regardless).
    pub start_delay: Duration,
    pub timeout: Duration,
    pub user_agent: String,
}

impl Default for CommunitySettings {
    fn default() -> Self {
        Self {
            anilist_url: ANILIST_GRAPHQL_URL.into(),
            jikan_url: JIKAN_MANGA_URL.into(),
            request_gap: Duration::from_secs(2),
            poll: Duration::from_secs(3600),
            start_delay: Duration::from_secs(60),
            timeout: Duration::from_secs(30),
            user_agent: format!(
                "mokuro-bunko/{} (+https://github.com/Gnathonic/mokuro-bunko)",
                bunko_core::VERSION
            ),
        }
    }
}

/// `(score, tags, genres)`.
pub type Normalized = (Option<f64>, Vec<String>, Vec<String>);

fn is_number(v: &Value) -> bool {
    v.is_number()
}

/// `normalize_anilist`: `meanScore` as-is (0–100), non-empty genres, tags with
/// `rank >= 40` in response order, at most 10.
pub fn normalize_anilist(media: &Value) -> Normalized {
    let score = media
        .get("meanScore")
        .filter(|v| is_number(v))
        .and_then(Value::as_f64);
    let genres = media
        .get("genres")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut tags: Vec<String> = Vec::new();
    for tag in media
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = tag
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if tag
            .get("rank")
            .and_then(Value::as_f64)
            .is_some_and(|rank| rank >= TAG_RANK_FLOOR)
        {
            tags.push(name.to_owned());
        }
    }
    tags.truncate(TAG_LIMIT);
    (score, tags, genres)
}

/// `normalize_jikan`: score ×10 rounded to one decimal; genres, themes and
/// demographics names, in that order; no tags.
pub fn normalize_jikan(data: &Value) -> Normalized {
    let score = data
        .get("score")
        .filter(|v| is_number(v))
        .and_then(Value::as_f64)
        .map(|s| (s * 10.0 * 10.0).round() / 10.0);
    let mut genres = Vec::new();
    for group in ["genres", "themes", "demographics"] {
        for entry in data
            .get(group)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                genres.push(name.to_owned());
            }
        }
    }
    (score, Vec::new(), genres)
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// `_is_fresh`: fetched less than [`REFRESH_AGE`] ago (unparsable = stale).
fn is_fresh(fetched_at: &str, now: f64) -> bool {
    let text = fetched_at
        .strip_suffix('Z')
        .map(|t| format!("{t}+00:00"))
        .unwrap_or_else(|| fetched_at.to_owned());
    match bunko_library::isodate::fromisoformat_utc_micros(&text) {
        Some(micros) => {
            now - bunko_library::isodate::micros_to_seconds(micros) < REFRESH_AGE.as_secs_f64()
        }
        None => false,
    }
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP Error {0}")]
    Status(u16),
    #[error("unexpected response shape")]
    Shape,
    #[error("stopped")]
    Stopped,
}

/// The background loop keeping `community_details` filled and fresh.
pub struct CommunityFetcher {
    db: Arc<Database>,
    settings: CommunitySettings,
    client: reqwest::Client,
    nudged: Mutex<HashSet<String>>,
    wake: Notify,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl CommunityFetcher {
    pub fn new(db: Arc<Database>, settings: CommunitySettings) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(settings.timeout)
            .user_agent(settings.user_agent.clone())
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            db,
            settings,
            client,
            nudged: Mutex::new(HashSet::new()),
            wake: Notify::new(),
            cancel: CancellationToken::new(),
            task: Mutex::new(None),
        })
    }

    /// Nudge: fetch this series soon, ignoring freshness (its external id changed).
    /// Wakes the loop at once, even during its start delay.
    pub fn request_fetch(&self, series_key: &str) {
        self.nudged.lock().insert(series_key.to_owned());
        self.wake.notify_one();
    }

    /// `(anilist-linked, mal-only)` series needing a fetch, key -> id.
    fn candidates(
        &self,
        only: Option<&HashSet<String>>,
        force: bool,
    ) -> bunko_db::Result<(HashMap<String, i64>, HashMap<String, i64>)> {
        let now = now_seconds();
        let fresh: HashSet<String> = if force {
            HashSet::new()
        } else {
            self.db
                .list_community_details()?
                .into_iter()
                .filter(|row| is_fresh(&row.fetched_at, now))
                .map(|row| row.series_key)
                .collect()
        };
        let mut anilist = HashMap::new();
        let mut mal_only = HashMap::new();
        for facts in self.db.list_series_facts()? {
            let key = facts.series_key;
            if fresh.contains(&key) || only.is_some_and(|only| !only.contains(&key)) {
                continue;
            }
            let int_id = |name: &str| facts.external_ids.get(name).and_then(Value::as_i64);
            if let Some(id) = int_id("anilist") {
                anilist.insert(key, id);
            } else if let Some(id) = int_id("mal") {
                mal_only.insert(key, id);
            }
        }
        Ok((anilist, mal_only))
    }

    async fn pause(&self, duration: Duration) -> bool {
        if duration.is_zero() {
            return !self.cancel.is_cancelled();
        }
        tokio::select! {
            _ = self.cancel.cancelled() => false,
            _ = tokio::time::sleep(duration) => true,
        }
    }

    /// One HTTP exchange; a 429 waits out its `Retry-After` (capped) and retries once.
    async fn http_json(&self, url: &str, body: Option<&Value>) -> Result<Value, FetchError> {
        for attempt in 0..2 {
            let mut request = match body {
                Some(body) => self.client.post(url).json(body),
                None => self.client.get(url),
            };
            request = request.header(reqwest::header::ACCEPT, "application/json");
            let response = tokio::select! {
                _ = self.cancel.cancelled() => return Err(FetchError::Stopped),
                r = request.send() => r?,
            };
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(Duration::from_secs(60), Duration::from_secs);
                if wait > MAX_RETRY_AFTER {
                    return Err(FetchError::Status(429));
                }
                tracing::info!(
                    "[COMMUNITY] rate limited by {url}; retrying in {}s",
                    wait.as_secs()
                );
                if !self.pause(wait).await {
                    return Err(FetchError::Stopped);
                }
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }
            let value = tokio::select! {
                _ = self.cancel.cancelled() => return Err(FetchError::Stopped),
                r = response.json::<Value>() => r?,
            };
            return Ok(value);
        }
        Err(FetchError::Status(429))
    }

    async fn store(&self, key: String, normalized: Normalized, source: &str) {
        let (score, tags, genres) = normalized;
        let row = CommunityDetails {
            series_key: key,
            score,
            tags: tags.into_iter().map(Value::String).collect(),
            genres: genres.into_iter().map(Value::String).collect(),
            source: source.to_owned(),
            fetched_at: bunko_library::isodate::iso_seconds_stamp(now_seconds())
                .unwrap_or_default(),
        };
        let db = self.db.clone();
        match tokio::task::spawn_blocking(move || db.upsert_community_details(&row)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "[COMMUNITY] storing details failed"),
            Err(error) => tracing::warn!(%error, "[COMMUNITY] storing details failed"),
        }
    }

    /// `run_once`: fetch and store details for every stale/missing linked series
    /// (`only` restricts the keys, `force` ignores freshness). Returns how many series
    /// were updated.
    pub async fn run_once(self: &Arc<Self>, only: Option<HashSet<String>>, force: bool) -> usize {
        let this = Arc::clone(self);
        let candidates =
            tokio::task::spawn_blocking(move || this.candidates(only.as_ref(), force)).await;
        let (anilist, mal_only) = match candidates {
            Ok(Ok(c)) => c,
            Ok(Err(error)) => {
                tracing::warn!(%error, "[COMMUNITY] cycle failed");
                return 0;
            }
            Err(error) => {
                tracing::warn!(%error, "[COMMUNITY] cycle failed");
                return 0;
            }
        };
        let mut updated = 0;
        let keys_by_id: HashMap<i64, String> = anilist.into_iter().map(|(k, id)| (id, k)).collect();
        let mut ids: Vec<i64> = keys_by_id.keys().copied().collect();
        ids.sort_unstable();
        for batch in ids.chunks(ANILIST_BATCH_SIZE) {
            if self.cancel.is_cancelled() {
                return updated;
            }
            let body = json!({"query": ANILIST_QUERY, "variables": {"ids": batch}});
            let media = match self
                .http_json(&self.settings.anilist_url, Some(&body))
                .await
            {
                Ok(payload) => match payload
                    .pointer("/data/Page/media")
                    .and_then(Value::as_array)
                {
                    Some(media) => media.clone(),
                    None => {
                        tracing::warn!(
                            "[COMMUNITY] AniList batch failed ({} ids): {}",
                            batch.len(),
                            FetchError::Shape
                        );
                        continue;
                    }
                },
                Err(FetchError::Stopped) => return updated,
                Err(error) => {
                    tracing::warn!(
                        "[COMMUNITY] AniList batch failed ({} ids): {error}",
                        batch.len()
                    );
                    continue;
                }
            };
            for item in &media {
                let Some(key) = item
                    .get("id")
                    .and_then(Value::as_i64)
                    .and_then(|id| keys_by_id.get(&id))
                else {
                    continue;
                };
                self.store(key.clone(), normalize_anilist(item), "anilist")
                    .await;
                updated += 1;
            }
            if !self.pause(self.settings.request_gap).await {
                return updated;
            }
        }
        let mut mal: Vec<(String, i64)> = mal_only.into_iter().collect();
        mal.sort();
        for (key, mal_id) in mal {
            if self.cancel.is_cancelled() {
                return updated;
            }
            let url = self.settings.jikan_url.replace("{id}", &mal_id.to_string());
            let data = match self.http_json(&url, None).await {
                Ok(payload) => match payload.get("data") {
                    Some(data) => data.clone(),
                    None => {
                        tracing::warn!(
                            "[COMMUNITY] Jikan fetch failed (mal {mal_id}): {}",
                            FetchError::Shape
                        );
                        continue;
                    }
                },
                Err(FetchError::Stopped) => return updated,
                Err(error) => {
                    tracing::warn!("[COMMUNITY] Jikan fetch failed (mal {mal_id}): {error}");
                    continue;
                }
            };
            self.store(key, normalize_jikan(&data), "mal").await;
            updated += 1;
            if !self.pause(self.settings.request_gap).await {
                return updated;
            }
        }
        if updated > 0 {
            tracing::info!("[COMMUNITY] updated community details for {updated} series");
        }
        updated
    }

    async fn run_loop(self: Arc<Self>) {
        let mut next_full = Instant::now() + self.settings.start_delay;
        loop {
            let woke = tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake.notified() => true,
                _ = tokio::time::sleep_until(next_full) => false,
            };
            if woke {
                let keys: HashSet<String> = std::mem::take(&mut *self.nudged.lock());
                if !keys.is_empty() {
                    self.run_once(Some(keys), true).await;
                }
                continue;
            }
            self.run_once(None, false).await;
            next_full = Instant::now() + self.settings.poll;
        }
    }

    /// Start the loop on the current tokio runtime (idempotent).
    pub fn start(self: &Arc<Self>) {
        let mut task = self.task.lock();
        if task.is_none() && !self.cancel.is_cancelled() {
            *task = Some(tokio::spawn(Arc::clone(self).run_loop()));
        }
    }

    /// Stop the loop; waits up to 5 s for an in-flight request to notice.
    pub async fn stop(&self) {
        self.cancel.cancel();
        let task = self.task.lock().take();
        if let Some(task) = task
            && tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
        {
            tracing::warn!("[COMMUNITY] fetcher did not stop within 5 s");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anilist_normalization() {
        let media = json!({"id": 1, "meanScore": 80, "genres": ["Action", "", 3],
            "tags": [{"name": "A", "rank": 90}, {"name": "B", "rank": 39}, {"name": "", "rank": 99}, {"name": "C", "rank": 40.0}]});
        assert_eq!(
            normalize_anilist(&media),
            (
                Some(80.0),
                vec!["A".into(), "C".into()],
                vec!["Action".into()]
            )
        );
        let many: Vec<Value> = (0..15)
            .map(|i| json!({"name": format!("t{i}"), "rank": 50}))
            .collect();
        assert_eq!(
            normalize_anilist(&json!({"tags": many, "meanScore": null}))
                .1
                .len(),
            10
        );
        assert_eq!(normalize_anilist(&json!({})).0, None);
    }

    #[test]
    fn jikan_normalization() {
        let data = json!({"score": 8.47, "genres": [{"name": "Drama"}], "themes": [{"name": "School"}], "demographics": [{"name": "Shounen"}, {}]});
        assert_eq!(
            normalize_jikan(&data),
            (
                Some(84.7),
                vec![],
                vec!["Drama".into(), "School".into(), "Shounen".into()]
            )
        );
        assert_eq!(normalize_jikan(&json!({"score": null})).0, None);
    }

    #[test]
    fn freshness() {
        let now = 1_760_000_000.0;
        let recent = bunko_library::isodate::iso_seconds_stamp(now - 3600.0).unwrap();
        let old = bunko_library::isodate::iso_seconds_stamp(now - 8.0 * 86400.0).unwrap();
        assert!(is_fresh(&recent, now));
        assert!(!is_fresh(&old, now));
        assert!(!is_fresh("garbage", now));
    }
}
