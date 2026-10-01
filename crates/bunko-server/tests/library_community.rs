//! The community fetcher against a local mock of AniList and Jikan.

mod library_support;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bunko_server::library::{CommunityFetcher, CommunitySettings};
use library_support::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Mock {
    anilist_bodies: Mutex<Vec<Value>>,
    user_agents: Mutex<Vec<String>>,
    jikan_calls: Mutex<Vec<i64>>,
    throttled_once: Mutex<bool>,
}

async fn anilist(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    mock.user_agents.lock().push(
        headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned(),
    );
    mock.anilist_bodies.lock().push(body.clone());
    let ids: Vec<i64> = body["variables"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    let media: Vec<Value> = ids
        .iter()
        .filter(|id| **id != 999) // an id AniList does not know is simply absent
        .map(|id| {
            json!({"id": id, "meanScore": 77, "genres": ["Adventure", ""],
                   "tags": [{"name": "Science", "rank": 91}, {"name": "Minor", "rank": 12}]})
        })
        .collect();
    Json(json!({"data": {"Page": {"media": media}}}))
}

async fn jikan(State(mock): State<Arc<Mock>>, Path(id): Path<i64>) -> Response {
    mock.jikan_calls.lock().push(id);
    {
        let mut throttled = mock.throttled_once.lock();
        if !*throttled {
            *throttled = true;
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "1")],
                "slow down",
            )
                .into_response();
        }
    }
    Json(json!({"data": {"score": 8.47, "genres": [{"name": "Drama"}], "themes": [{"name": "School"}], "demographics": []}}))
        .into_response()
}

async fn serve_mock(mock: Arc<Mock>) -> String {
    let app = Router::new()
        .route("/graphql", post(anilist))
        .route("/jikan/{id}", get(jikan))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn settings(base: &str) -> CommunitySettings {
    CommunitySettings {
        anilist_url: format!("{base}/graphql"),
        jikan_url: format!("{base}/jikan/{{id}}"),
        request_gap: Duration::ZERO,
        poll: Duration::from_secs(3600),
        start_delay: Duration::from_secs(3600),
        timeout: Duration::from_secs(5),
        ..CommunitySettings::default()
    }
}

fn facts(key: &str, ids: Value) -> bunko_db::SeriesFacts {
    bunko_db::SeriesFacts {
        series_key: key.into(),
        series_title: key.into(),
        external_ids: ids.as_object().unwrap().clone(),
        facts_updated_at: "2026-08-18T19:36:24.324Z".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cycle_fetches_anilist_in_batches_and_mal_only_series_from_jikan() {
    let env = Env::new(|_| {});
    let mock = Arc::new(Mock::default());
    let base = serve_mock(mock.clone()).await;
    env.db
        .put_series_facts(&facts("dr stone", json!({"anilist": 98416, "mal": 103897})))
        .unwrap();
    env.db
        .put_series_facts(&facts("aria", json!({"mal": 4})))
        .unwrap();
    env.db
        .put_series_facts(&facts("ghost", json!({"anilist": 999})))
        .unwrap();
    env.db
        .put_series_facts(&facts("unlinked", json!({})))
        .unwrap();
    for i in 0..60 {
        env.db
            .put_series_facts(&facts(
                &format!("bulk {i:02}"),
                json!({"anilist": 1000 + i}),
            ))
            .unwrap();
    }

    let fetcher = CommunityFetcher::new(env.db.clone(), settings(&base));
    let updated = fetcher.run_once(None, false).await;
    assert_eq!(updated, 62); // 61 AniList (not the unknown id) + 1 Jikan

    let bodies = mock.anilist_bodies.lock().clone();
    assert_eq!(bodies.len(), 2, "62 ids in batches of 50");
    let first: Vec<i64> = bodies[0]["variables"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    assert_eq!(first.len(), 50);
    assert!(first.windows(2).all(|w| w[0] < w[1]), "ids sorted");
    assert!(
        bodies[0]["query"]
            .as_str()
            .unwrap()
            .contains("media(id_in: $ids, type: MANGA)")
    );
    assert!(mock.user_agents.lock()[0].starts_with("mokuro-bunko/"));
    // The 429 was waited out and retried.
    assert_eq!(*mock.jikan_calls.lock(), vec![4, 4]);

    let rows = env.db.list_community_details().unwrap();
    let dr = rows.iter().find(|r| r.series_key == "dr stone").unwrap();
    assert_eq!(dr.score, Some(77.0));
    assert_eq!(dr.tags, vec![json!("Science")]);
    assert_eq!(dr.genres, vec![json!("Adventure")]);
    assert_eq!(dr.source, "anilist");
    let aria = rows.iter().find(|r| r.series_key == "aria").unwrap();
    assert_eq!((aria.score, aria.source.as_str()), (Some(84.7), "mal"));
    assert_eq!(aria.genres, vec![json!("Drama"), json!("School")]);
    assert!(
        !rows
            .iter()
            .any(|r| r.series_key == "ghost" || r.series_key == "unlinked")
    );

    // Everything stored is fresh now: the next sweep asks only for the id AniList did
    // not return last time (0.5.2 keeps retrying it every cycle) and stores nothing.
    assert_eq!(fetcher.run_once(None, false).await, 0);
    let bodies = mock.anilist_bodies.lock().clone();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[2]["variables"]["ids"], json!([999]));
    assert_eq!(mock.jikan_calls.lock().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nudge_fetches_at_once_even_during_the_start_delay() {
    let env = Env::new(|_| {});
    let mock = Arc::new(Mock::default());
    let base = serve_mock(mock.clone()).await;
    env.db
        .put_series_facts(&facts("dr stone", json!({"anilist": 98416})))
        .unwrap();
    env.db
        .put_series_facts(&facts("other", json!({"anilist": 5})))
        .unwrap();
    // A fresh row would normally be skipped; a nudge ignores freshness.
    env.db
        .upsert_community_details(&bunko_db::CommunityDetails {
            series_key: "dr stone".into(),
            score: Some(1.0),
            source: "anilist".into(),
            fetched_at: bunko_library::isodate::iso_seconds_stamp(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64(),
            )
            .unwrap(),
            ..Default::default()
        })
        .unwrap();
    let fetcher = CommunityFetcher::new(env.db.clone(), settings(&base));
    fetcher.start();
    fetcher.request_fetch("dr stone");
    let db = env.db.clone();
    let got = wait_for(Duration::from_secs(5), || {
        db.list_community_details()
            .unwrap()
            .iter()
            .any(|r| r.series_key == "dr stone" && r.score == Some(77.0))
    })
    .await;
    assert!(got, "nudge not served");
    // Only the nudged series was fetched (the full sweep is still 1 h away).
    let ids: Vec<Value> = mock
        .anilist_bodies
        .lock()
        .iter()
        .map(|b| b["variables"]["ids"].clone())
        .collect();
    assert_eq!(ids, vec![json!([98416])]);
    fetcher.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_id_change_nudges_the_running_fetcher() {
    let mock = Arc::new(Mock::default());
    let base = serve_mock(mock.clone()).await;
    let env = Env::with(
        |c| c.catalog.enrich_community = true,
        Options {
            community: Some(settings(&base)),
            ..Options::default()
        },
    );
    let dir = env.series_dir("Dr Stone");
    write_cbz(&dir.join("v1.cbz"), 1);
    env.runtime.start();
    assert!(
        env.runtime.community().is_some(),
        "started with catalog.enabled && enrich_community"
    );
    let update =
        br#"{"version":2,"updated_at":"2026-01-01T00:00:00Z","external_ids":{"anilist":98416}}"#;
    assert!(
        env.runtime
            .on_series_put("Dr Stone", update.to_vec(), Some("ed"))
            .await
            .unwrap()
    );
    let db = env.db.clone();
    assert!(
        wait_for(Duration::from_secs(5), || db
            .list_community_details()
            .unwrap()
            .iter()
            .any(|r| r.series_key == "dr stone"))
        .await,
        "the id change did not reach the fetcher"
    );
    // The same ids again: accepted, but no nudge (nothing changed).
    let before = mock.anilist_bodies.lock().len();
    assert!(
        env.runtime
            .on_series_put("Dr Stone", update.to_vec(), Some("ed"))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.anilist_bodies.lock().len(), before);
    env.runtime.stop().await;
    assert!(env.runtime.community().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_fetcher_without_enrichment() {
    let env = Env::new(|c| c.catalog.enrich_community = false);
    env.runtime.start();
    assert!(env.runtime.community().is_none());
    env.runtime.stop().await;
}
