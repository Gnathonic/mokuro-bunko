//! The public catalog API and the volume manifest.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod library_support;

use axum::body::Body;
use bunko_core::Role;
use bunko_server::library::{OcrStatusSource, OutlookSource, VolumeOutlook};
use library_support::*;
use serde_json::{Map, Value, json};
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

async fn catalog_env(edit: impl FnOnce(&mut bunko_core::Config)) -> Env {
    let env = Env::new(edit);
    let dir = env.series_dir("Dr Stone");
    write_cbz(&dir.join("Volume 01.cbz"), 3);
    write_mokuro(&dir.join("Volume 01.mokuro"), "uuid-1", 4); // names a page the archive lacks
    std::fs::write(dir.join("Volume 01.webp"), b"RIFFwebp").unwrap();
    write_cbz(&dir.join("Volume 02.cbz"), 2);
    let aria = env.series_dir("Aria");
    write_cbz(&aria.join("Aria 01.cbz"), 1);
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_listing_falls_back_to_the_index_then_reads_the_materialized_rows() {
    let env = catalog_env(|_| {}).await;
    // Before any pass: the index fallback (no totals).
    let first = env.send(get("/catalog/api/library")).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.header("content-type"), Some("application/json"));
    assert_eq!(
        first.text(),
        r#"{"series": [{"name": "Aria", "path": "Aria", "cover": null, "volume_count": 1}, {"name": "Dr Stone", "path": "Dr Stone", "cover": "Dr Stone/Volume 01.webp", "volume_count": 2}]}"#
    );

    assert!(env.runtime.recompile_all_now().await > 0);
    // Facts and community details decorate the rows.
    let facts = bunko_db::SeriesFacts {
        series_key: "dr stone".into(),
        series_title: "Dr Stone".into(),
        titles: json!({"native": "ドクターストーン"})
            .as_object()
            .unwrap()
            .clone(),
        tag: Some("HD".into()),
        facts_updated_at: "2026-08-18T19:36:24.324Z".into(),
        ..Default::default()
    };
    env.db.put_series_facts(&facts).unwrap();
    env.db
        .upsert_community_details(&bunko_db::CommunityDetails {
            series_key: "dr stone".into(),
            score: Some(80.0),
            tags: vec![json!("Science")],
            genres: vec![json!("Adventure")],
            source: "anilist".into(),
            fetched_at: "2026-09-01T00:00:00Z".into(),
        })
        .unwrap();
    let r = env.send(get("/catalog/api/library")).await;
    let text = r.text();
    let modified = env.db.list_catalog_series().unwrap()[1].latest_volume_modified;
    let expected = format!(
        concat!(
            r#"{{"series": [{{"name": "Aria", "path": "Aria", "cover": null, "volume_count": 1, "latest_volume_modified": {aria}, "total_pages": 1, "total_chars": 0, "missing_pages": 0, "damaged_volumes": 0}}, "#,
            r#"{{"name": "Dr Stone", "path": "Dr Stone", "cover": "Dr Stone/Volume 01.webp", "volume_count": 2, "latest_volume_modified": {dr}, "total_pages": 6, "total_chars": 20, "missing_pages": 1, "damaged_volumes": 1, "#,
            "\"titles\": {{\"native\": \"\\u30c9\\u30af\\u30bf\\u30fc\\u30b9\\u30c8\\u30fc\\u30f3\"}}, ",
            r#""tag": "HD", "community": {{"score": 80.0, "tags": ["Science"], "genres": ["Adventure"], "source": "anilist"}}}}]}}"#
        ),
        aria = bunko_db::pyfmt::float_repr(
            env.db.list_catalog_series().unwrap()[0].latest_volume_modified
        ),
        dr = bunko_db::pyfmt::float_repr(modified),
    );
    assert_eq!(text, expected);
    assert!(
        r.header("content-encoding").is_none(),
        "an un-asked-for body is never gzipped"
    );

    let gz = env
        .send(
            req("GET", "/catalog/api/library")
                .header("accept-encoding", "gzip, br")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    assert_eq!(gz.header("vary"), Some("Accept-Encoding"));
    let mut plain = String::new();
    flate2::read::GzDecoder::new(&gz.body[..])
        .read_to_string(&mut plain)
        .unwrap();
    assert_eq!(plain, expected);
}

struct Progress;

impl OcrStatusSource for Progress {
    fn progress(&self) -> Option<Map<String, Value>> {
        json!({"active": true, "series": "Dr Stone", "volume": "Volume 02", "percent": 40, "eta_seconds": 12,
               "jobs": [{"relative_cbz": "dr stone/VOLUME 02.cbz", "percent": 40, "eta_seconds": 12, "status": "ocr",
                         "processed_pages": 1, "total_pages": 2}]})
        .as_object()
        .cloned()
    }
}

/// Volume 02 keeps failing: nothing in it could be decoded.
struct Failing;

impl OcrStatusSource for Failing {
    fn progress(&self) -> Option<Map<String, Value>> {
        None
    }

    fn volume_failure(&self, series: &str, volume: &str) -> Option<String> {
        (series == "Dr Stone" && volume == "Volume 02")
            .then(|| "This volume has no pages that could be decoded.".to_string())
    }
}

/// A volume whose OCR keeps failing is "OCR failed", with the public text of the failure
/// for the badge's hover text, instead of "OCR pending"; the other volumes' entries are
/// unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_volume_says_so_instead_of_pending() {
    let mut env = catalog_env(|_| {}).await;
    assert!(env.runtime.recompile_all_now().await > 0);
    env.deps.ocr_status = Some(Arc::new(Failing));
    let series = env
        .send(get("/catalog/api/series?name=Dr+Stone"))
        .await
        .json();
    let v2 = &series["volumes"][1];
    assert_eq!(v2["ocr_pending"], false);
    assert_eq!(v2["ocr_failed"], true);
    assert_eq!(
        v2["ocr_failure"],
        "This volume has no pages that could be decoded."
    );
    let v1 = series["volumes"][0].as_object().unwrap();
    assert!(!v1.contains_key("ocr_failed") && !v1.contains_key("ocr_failure"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn series_page_config_status_cover_and_static() {
    let mut env = catalog_env(|c| c.catalog.reader_url = "https://reader.example".into()).await;
    assert!(env.runtime.recompile_all_now().await > 0);

    let config = env.send(get("/catalog/api/config")).await;
    assert_eq!(config.text(), r#"{"reader_url": "https://reader.example"}"#);
    let idle = env.send(get("/catalog/api/ocr-status")).await;
    assert_eq!(idle.text(), r#"{"active": false}"#);

    let series = env.send(get("/catalog/api/series?name=Dr+Stone")).await;
    assert_eq!(series.status, 200);
    assert_eq!(
        series.text(),
        r#"{"name": "Dr Stone", "cover": "Dr Stone/Volume 01.webp", "volumes": [{"name": "Volume 01", "cover": "Dr Stone/Volume 01.webp", "ocr_pending": false, "ocr_active": false, "page_count": 4, "missing_pages": 1}, {"name": "Volume 02", "cover": null, "ocr_pending": true, "ocr_active": false, "page_count": 2, "missing_pages": 0}]}"#
    );
    let by_path = env.send(get("/catalog/api/series/Dr%20Stone")).await;
    assert_eq!(by_path.text(), series.text());
    // Any spelling of the name is that series, on every host (the library is
    // case-insensitive): the answer names it as the library spells it.
    for respelled in [
        "/catalog/api/series?name=dr+stone",
        "/catalog/api/series/DR%20STONE",
    ] {
        let r = env.send(get(respelled)).await;
        assert_eq!((r.status, r.text()), (200, series.text()), "{respelled}");
    }
    assert_eq!(
        env.send(get("/catalog/api/series")).await.text(),
        r#"{"error": "Missing series name"}"#
    );
    let unknown = env.send(get("/catalog/api/series?name=Nope")).await;
    assert_eq!(
        (unknown.status, unknown.text().as_str()),
        (404, r#"{"error": "Series not found"}"#)
    );
    let escape = env.send(get("/catalog/api/series?name=../..")).await;
    assert_eq!(
        (escape.status, escape.text().as_str()),
        (403, r#"{"error": "Forbidden"}"#)
    );

    // With OCR running on Volume 02.
    env.deps.ocr_status = Some(Arc::new(Progress));
    let active = env
        .send(get("/catalog/api/series?name=Dr%20Stone"))
        .await
        .json();
    assert_eq!(active["volumes"][1]["ocr_pending"], false);
    assert_eq!(active["volumes"][1]["ocr_active"], true);
    assert_eq!(
        active["volumes"][1]["ocr_progress"],
        json!({"percent": 40, "eta_seconds": 12, "status": "ocr", "processed_pages": 1, "total_pages": 2})
    );
    let status = env.send(get("/catalog/api/ocr-status")).await.json();
    assert_eq!(status["active"], true);
    assert_eq!(status["volume"], "Volume 02");

    let cover = env
        .send(get("/catalog/api/cover?path=Dr%20Stone/Volume%2001.webp"))
        .await;
    assert_eq!(cover.status, 200);
    assert_eq!(&cover.body[..], b"RIFFwebp");
    assert_eq!(cover.header("content-type"), Some("image/webp"));
    for respelled in [
        "/catalog/api/cover?path=dr%20stone/VOLUME%2001.webp",
        "/catalog/api/cover/DR%20STONE/volume%2001.webp",
    ] {
        let r = env.send(get(respelled)).await;
        assert_eq!(
            (r.status, &r.body[..]),
            (200, &b"RIFFwebp"[..]),
            "{respelled}"
        );
    }
    // Revalidated on every use: a cover remade under the same name shows at once.
    assert_eq!(cover.header("cache-control"), Some("no-cache"));
    let etag = cover.header("etag").expect("an ETag").to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert!(cover.header("last-modified").is_some());
    let same = env
        .send(
            req("GET", "/catalog/api/cover?path=Dr%20Stone/Volume%2001.webp")
                .header("if-none-match", etag.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(same.status, 304);
    assert!(same.body.is_empty());
    assert_eq!(same.header("etag"), Some(etag.as_str()));
    assert_eq!(same.header("cache-control"), Some("no-cache"));
    let other = env
        .send(
            req("GET", "/catalog/api/cover?path=Dr%20Stone/Volume%2001.webp")
                .header("if-none-match", "\"something-else\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(other.status, 200);
    assert_eq!(&other.body[..], b"RIFFwebp");
    // A remade cover is another version.
    let file = env.series_dir("Dr Stone").join("Volume 01.webp");
    std::fs::write(&file, b"RIFFnewer").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
        .unwrap();
    let remade = env
        .send(
            req("GET", "/catalog/api/cover?path=Dr%20Stone/Volume%2001.webp")
                .header("if-none-match", etag.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(remade.status, 200);
    assert_eq!(&remade.body[..], b"RIFFnewer");
    assert_ne!(remade.header("etag"), Some(etag.as_str()));
    assert_eq!(
        env.send(get("/catalog/api/cover/Dr%20Stone/Volume%2001.webp"))
            .await
            .status,
        200
    );
    assert_eq!(
        env.send(get("/catalog/api/cover?path=Dr%20Stone/Volume%2001.cbz"))
            .await
            .status,
        403
    );
    assert_eq!(
        env.send(get("/catalog/api/cover?path=Dr%20Stone/none.webp"))
            .await
            .status,
        404
    );
    assert_eq!(
        env.send(get("/catalog/api/cover?path=../../etc/passwd"))
            .await
            .status,
        403
    );
    let missing = env.send(get("/catalog/api/cover")).await;
    assert_eq!(
        (missing.status, missing.text().as_str()),
        (400, "Missing cover path")
    );
    assert_eq!(missing.header("content-type"), Some("text/plain"));
    let other = env.send(get("/catalog/api/nothing")).await;
    assert_eq!(
        (other.status, other.text().as_str()),
        (404, r#"{"error": "Not found"}"#)
    );
    assert_eq!(
        env.send(
            req("POST", "/catalog/api/library")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        404
    );

    let index = env.send(get("/catalog")).await;
    assert_eq!(index.status, 200);
    assert_eq!(
        index.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(index.header("cache-control"), Some("no-cache"));
    let js = env.send(get("/catalog/catalog.js")).await;
    assert_eq!(
        js.header("content-type"),
        Some("application/javascript; charset=utf-8")
    );
    let spa = env.send(get("/catalog/some/route")).await;
    assert_eq!((spa.status, spa.body.clone()), (200, index.body.clone()));
    assert_eq!(env.send(get("/catalog/../etc")).await.status, 403);

    // Off: everything but the manifest falls through to the rest of the app.
    env.core.config.write().catalog.enabled = false;
    assert_eq!(env.send(get("/catalog/")).await.status, 418);
    assert_eq!(env.send(get("/catalog/api/library")).await.status, 418);
}

struct Outlook;

impl OutlookSource for Outlook {
    fn volume_outlook(&self, cbz: &Path, series: &str, volume: &str) -> VolumeOutlook {
        if volume != "Volume 02" {
            return VolumeOutlook::default();
        }
        assert!(cbz.ends_with("Dr Stone/Volume 02.cbz"));
        assert_eq!(series, "Dr Stone");
        VolumeOutlook {
            pending: vec![json!({"id": "hayai", "eta": "2026-10-01T00:00:00Z"})],
            recheck_after: Some(json!(42)),
        }
    }
}

/// D27: personal corrections are one account's own, so the manifest (anonymous when
/// downloads are) never lists one; the shared curated layer and engine layers stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_never_lists_personal_corrections() {
    let env = catalog_env(|c| c.registration.allow_anonymous_download = true).await;
    let dir = env.series_dir("Dr Stone");
    for id in ["hayai", "curated", "edit", "edit-2", "mine"] {
        write_mokuro(&dir.join(format!("Volume 01.{id}.mokuro")), "uuid-1", 3);
    }
    let m = env
        .send(get(
            "/catalog/api/manifest?series=Dr%20Stone&volume=Volume%2001",
        ))
        .await
        .json();
    let ids: Vec<&str> = m["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["curated", "hayai", "mine"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_is_gated_like_the_archive() {
    let mut env = catalog_env(|c| {
        c.registration.allow_anonymous_download = false;
        c.catalog.enabled = false; // the manifest does not care
    })
    .await;
    env.user("reader", Role::Registered);
    assert!(env.runtime.recompile_all_now().await > 0);
    let uri = "/catalog/api/manifest?series=Dr%20Stone&volume=Volume%2001";

    let anonymous = env.send(get(uri)).await;
    assert_eq!(anonymous.status, 401);
    assert_eq!(anonymous.text(), "Authentication required");
    assert!(
        anonymous
            .header("www-authenticate")
            .is_some_and(|v| v.starts_with("Basic"))
    );
    let missing = env
        .send(get("/catalog/api/manifest?series=Dr%20Stone"))
        .await;
    assert_eq!(
        (missing.status, missing.text().as_str()),
        (400, r#"{"error": "Missing series or volume"}"#)
    );

    let authed = |uri: &str| {
        req("GET", uri)
            .header("authorization", basic("reader"))
            .body(Body::empty())
            .unwrap()
    };
    let r = env.send(authed(uri)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let m = r.json();
    assert_eq!(m["version"], 1);
    assert_eq!(m["series"], "Dr Stone");
    assert_eq!(
        m["archive"]["url"],
        "/mokuro-reader/Dr%20Stone/Volume%2001.cbz"
    );
    assert_eq!(
        m["ocr"]["url"],
        "/mokuro-reader/Dr%20Stone/Volume%2001.mokuro"
    );
    let sidecar = std::fs::read(env.library.join("Dr Stone/Volume 01.mokuro")).unwrap();
    let digest = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(&sidecar))
    };
    assert_eq!(
        m["ocr"]["sha256"],
        digest.as_str(),
        "cached hash of the current sidecar"
    );
    assert_eq!(
        m["cover"]["url"],
        "/mokuro-reader/Dr%20Stone/Volume%2001.webp"
    );
    assert_eq!(
        m["series_file"]["url"],
        "/mokuro-reader/Dr%20Stone/series.json"
    );
    assert_eq!(m["layers"], json!([]));
    assert_eq!(m["pending"], json!([]));
    assert_eq!(m["recheck_after"], Value::Null);
    assert!(r.text().starts_with(
        r#"{"version": 1, "series": "Dr Stone", "volume": "Volume 01", "archive": {"url": "#
    ));

    // Pending OCR comes from the outlook source.
    env.deps.outlook = Some(Arc::new(Outlook));
    let pending = env
        .send(authed(
            "/catalog/api/manifest?series=Dr%20Stone&volume=Volume%2002",
        ))
        .await
        .json();
    assert_eq!(pending["ocr"], Value::Null);
    assert_eq!(
        pending["pending"],
        json!([{"id": "hayai", "eta": "2026-10-01T00:00:00Z"}])
    );
    assert_eq!(pending["recheck_after"], 42);

    let absent = env
        .send(authed(
            "/catalog/api/manifest?series=Dr%20Stone&volume=Nope",
        ))
        .await;
    assert_eq!(
        (absent.status, absent.text().as_str()),
        (404, r#"{"error": "Volume not found"}"#)
    );
    let nested = env
        .send(authed("/catalog/api/manifest?series=Dr%20Stone&volume=a/b"))
        .await;
    assert_eq!(nested.status, 404);
    let escape = env
        .send(authed("/catalog/api/manifest?series=../..&volume=x"))
        .await;
    assert_eq!(escape.status, 403);

    // Anonymous downloads on: the manifest opens up with them.
    env.core
        .config
        .write()
        .registration
        .allow_anonymous_download = true;
    assert_eq!(env.send(get(uri)).await.status, 200);
}

/// A library folder spelled like a per-user name (`history/`, a legacy folder or the
/// cloud-root owner's own tree) is never the catalog's, in any spelling, on any host:
/// not listed (the index fallback included), its page, covers and manifest 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_per_user_named_folder_is_never_in_the_catalog() {
    let env = catalog_env(|_| {}).await;
    let dir = env.series_dir("history");
    write_cbz(&dir.join("v.cbz"), 1);
    std::fs::write(dir.join("v.webp"), b"RIFFwebp").unwrap();
    let listing = env.send(get("/catalog/api/library")).await;
    assert_eq!(listing.status, 200);
    assert!(!listing.text().contains("history"), "{}", listing.text());
    for path in [
        "/catalog/api/cover/history/v.webp",
        "/catalog/api/cover/HISTORY/v.webp",
        "/catalog/api/cover/Aria/../history/v.webp",
        "/catalog/api/series/history",
        "/catalog/api/series?name=History",
    ] {
        let r = env.send(get(path)).await;
        assert_eq!(r.status, 404, "{path}: {}", r.text());
    }
    assert!(env.runtime.recompile_all_now().await > 0);
    let listing = env.send(get("/catalog/api/library")).await;
    assert!(!listing.text().contains("history"), "{}", listing.text());
}

/// A hidden archive's own cover (`v.webp` beside the hidden `v.cbz`, where the disk
/// keeps both spellings) is never served, through any spelling; a cover the listings
/// reach is, in any spelling (`V.webp` beside `V.cbz` only).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_archives_cover_is_never_served() {
    let env = catalog_env(|_| {}).await;
    let dir = env.series_dir("S");
    write_cbz(&dir.join("V.cbz"), 1);
    let two = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("v.cbz"))
        .is_ok();
    std::fs::write(dir.join("v.webp"), b"RIFFwebp").unwrap();
    for path in ["/catalog/api/cover/S/v.webp", "/catalog/api/cover/s/V.WEBP"] {
        let r = env.send(get(path)).await;
        if two {
            assert_eq!(r.status, 404, "{path}: {}", r.text());
        } else {
            assert_eq!(r.status, 200, "{path}: {}", r.text());
        }
    }
}

/// Two folders that fold alike (only a case-sensitive disk holds both): the series view
/// answers the winner, the byte-smallest, for either spelling, once the walk has listed
/// the library as well as before. The exact spelling of the shadowed folder is never its
/// own series: the walk does not list it, so it is resolved from the root's listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shadowed_spelling_of_a_series_answers_its_winner() {
    let env = Env::new(|_| {});
    let winner = env.series_dir("Saga");
    write_cbz(&winner.join("Saga 01.cbz"), 1);
    let shadowed = env.library.join("saga");
    if std::fs::create_dir(&shadowed).is_err() || shadowed.join("Saga 01.cbz").exists() {
        return; // a case-insensitive disk: one folder.
    }
    write_cbz(&shadowed.join("saga 09.cbz"), 1);
    let names = |r: &Value| -> (String, Vec<String>) {
        (
            r["name"].as_str().unwrap().to_string(),
            r["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["name"].as_str().unwrap().to_string())
                .collect(),
        )
    };
    let want = ("Saga".to_string(), vec!["Saga 01".to_string()]);
    for walked in [false, true] {
        if walked {
            env.runtime.index().get_snapshot_fresh();
            assert!(env.runtime.index().lists_series("Saga"));
            assert!(!env.runtime.index().lists_series("saga"));
        }
        for spelled in ["Saga", "saga", "SAGA"] {
            let r = env
                .send(get(&format!("/catalog/api/series?name={spelled}")))
                .await;
            assert_eq!(r.status, 200, "{spelled} walked={walked}");
            assert_eq!(names(&r.json()), want, "{spelled} walked={walked}");
        }
    }
}
