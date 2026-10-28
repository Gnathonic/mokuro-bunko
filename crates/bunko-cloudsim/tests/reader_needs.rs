//! What the reader's providers send that Bunko's drivers do not, each as the reader
//! sends it: Drive queries with `or` and parentheses, `description`, a resumable
//! upload with no declared size and a tokenless single PUT; Graph root-path
//! addressing and `replace` upload sessions, children without `$top`, and a
//! download redirect to another origin; MEGA's binary completion token (`v: 2`).

use base64::Engine;
use bunko_cloudsim::{CloudSim, Provider};
use serde_json::{Value, json};

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn drive_list(sim: &CloudSim, token: &str, q: &str) -> (u16, Vec<String>) {
    let r = http()
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth(token)
        .query(&[("q", q)])
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    let mut names: Vec<String> = v["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| f["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    (status, names)
}

#[tokio::test]
async fn drive_queries_with_or_parentheses_and_not() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Google, "r").access_token;
    let d = sim.drive("r");
    let root = d.mkdir("root", "mokuro-reader");
    let other = d.mkdir("root", "Other");
    d.put(&root, "catalog.json", b"[]");
    d.put(&other, "catalog.json", b"[]");
    let trashed = d.put(&root, "old.json", b"x");
    d.lock().files.get_mut(&trashed).unwrap().trashed = true;

    let q = format!(
        "name='catalog.json' and trashed=false and ('{root}' in parents or '{other}' in parents)"
    );
    assert_eq!(drive_list(&sim, &t, &q).await.1.len(), 2);
    let q = format!(
        "'{root}' in parents and (mimeType='application/zip' or mimeType='application/vnd.google-apps.folder') and trashed=false"
    );
    assert_eq!(drive_list(&sim, &t, &q).await.1, Vec::<String>::new());
    let q = "name='mokuro-reader' and mimeType='application/vnd.google-apps.folder' and 'root' in parents and trashed=false";
    assert_eq!(drive_list(&sim, &t, q).await.1, ["mokuro-reader"]);
    let q = format!("'{root}' in parents and not trashed=false");
    assert_eq!(drive_list(&sim, &t, &q).await.1, ["old.json"]);
    let q = format!("'{root}' in parents and name != 'catalog.json' and trashed = false");
    assert_eq!(drive_list(&sim, &t, &q).await.1, Vec::<String>::new());
    // Escapes in names.
    d.put(&root, "Haven't \\ You", b"x");
    let q = format!(r"name='Haven\'t \\ You' and '{root}' in parents");
    assert_eq!(drive_list(&sim, &t, &q).await.1, ["Haven't \\ You"]);
    // Unmodelled syntax and unscoped queries are refused and recorded.
    assert_eq!(drive_list(&sim, &t, "fullText contains 'x'").await.0, 400);
    assert_eq!(drive_list(&sim, &t, "trashed=false").await.0, 400);
    assert_eq!(
        drive_list(&sim, &t, &format!("'{root}' in parents and (trashed=false"))
            .await
            .0,
        400
    );
    let v = sim.take_violations();
    assert_eq!(v.len(), 3, "{v:?}");
    assert!(v[0].contains("fullText contains 'x'"), "{}", v[0]);
    assert!(v[1].contains("would list the whole drive"), "{}", v[1]);
    assert!(v[2].contains("missing `)`"), "{}", v[2]);
}

#[tokio::test]
async fn drive_resumable_upload_without_size_or_token_and_a_description() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Google, "r").access_token;
    let folder = sim.drive("r").mkdir("root", "mokuro-reader");
    let c = http();
    // No X-Upload-Content-Length; the size comes with the data.
    let r = c
        .post(format!("{}/upload/drive/v3/files", sim.drive_api_base()))
        .query(&[("uploadType", "resumable"), ("fields", "id,modifiedTime,size")])
        .bearer_auth(&t)
        .json(&json!({"name": "v.cbz", "mimeType": "application/x-cbz", "parents": [folder], "description": "Series A"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    // The session URL is the credential: no Authorization, no Content-Range.
    let r = c
        .put(&loc)
        .body(b"whole file".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["size"], "10");
    assert_eq!(v["description"], "Series A");
    assert_eq!(v["mimeType"], "application/x-cbz");
    // A description PATCH, as the reader stores a verified series title.
    let id = v["id"].as_str().unwrap();
    let r = c
        .patch(format!("{}/drive/v3/files/{id}", sim.drive_api_base()))
        .bearer_auth(&t)
        .json(&json!({"description": "Series B"}))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["description"], "Series B");
    // A replace (PATCH session) without keepRevisionForever, in two parts with `*`.
    let r = c
        .patch(format!(
            "{}/upload/drive/v3/files/{id}",
            sim.drive_api_base()
        ))
        .query(&[("uploadType", "resumable")])
        .bearer_auth(&t)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let r = c
        .put(&loc)
        .header("Content-Range", "bytes 0-3/*")
        .body(b"abcd".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 308);
    let r = c
        .put(&loc)
        .header("Content-Range", "bytes 4-5/6")
        .body(b"ef".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let files = sim.files(Provider::Google, "r");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].size, 6);
    // A session URL of an unknown account is refused without a token.
    let bad = loc.replace("upload_id=", "upload_id=00");
    let r = c.put(&bad).body(b"x".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 401);
    sim.assert_clean();
}

#[tokio::test]
async fn graph_root_paths_replace_sessions_and_listing_without_top() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Microsoft, "r").access_token;
    sim.seed(
        Provider::Microsoft,
        "r",
        &[("mokuro-reader/Series A/v1.cbz".into(), b"one".to_vec())],
    );
    let g = sim.graph_api_base();
    let c = http();
    let get = |url: String| {
        let c = c.clone();
        let t = t.clone();
        async move { c.get(url).bearer_auth(t).send().await.unwrap() }
    };
    // Item by path, with or without the closing colon; names fold.
    let r = get(format!("{g}/me/drive/root:/mokuro-reader/series%20a")).await;
    assert_eq!(r.status(), 200);
    let r = get(format!(
        "{g}/me/drive/root:/mokuro-reader/Series%20A/missing"
    ))
    .await;
    assert_eq!(r.status(), 404);
    // Children by path, and of the root, without $top.
    let v: Value = get(format!(
        "{g}/me/drive/root:/mokuro-reader/Series%20A:/children"
    ))
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(v["value"][0]["name"], "v1.cbz");
    let v: Value = get(format!("{g}/me/drive/root/children"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["value"][0]["name"], "mokuro-reader");
    // A folder by path.
    let r = c
        .post(format!("{g}/me/drive/root:/mokuro-reader:/children"))
        .bearer_auth(&t)
        .json(
            &json!({"name": "Series B", "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    // An upload session by path with `replace` overwrites the file in place.
    let r = c
        .post(format!(
            "{g}/me/drive/root:/mokuro-reader/Series%20A/v1.cbz:/createUploadSession"
        ))
        .bearer_auth(&t)
        .json(&json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}}))
        .send()
        .await
        .unwrap();
    let url = r.json::<Value>().await.unwrap()["uploadUrl"]
        .as_str()
        .unwrap()
        .to_string();
    // The pre-authenticated URLs are on the other loopback name.
    assert!(url.starts_with("http://localhost:"), "{url}");
    let r = c
        .put(&url)
        .header("Content-Range", "bytes 0-4/5")
        .body(b"two!!".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let files = sim.files(Provider::Microsoft, "r");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].size, 5);
    // The download redirect points at that other origin.
    let id = r.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = get(format!("{g}/me/drive/items/{id}/content")).await;
    assert_eq!(r.status(), 302);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    assert!(loc.starts_with("http://localhost:"), "{loc}");
    assert_eq!(
        c.get(&loc)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"two!!"
    );
    sim.assert_clean();
}

#[tokio::test]
async fn mega_v2_uploads_answer_a_binary_completion_token() {
    let sim = bunko_cloudsim::start().await;
    let m = sim.mega("r");
    let sid = {
        // The session id is inside the blob; log in instead, as megajs does.
        m.set_account("r@cloudsim.test", "pw", None);
        let mut cfg = bunko_mega::ClientConfig {
            api_origin: sim.mega_origin(),
            ..bunko_mega::ClientConfig::default()
        };
        cfg.min_cs_interval = std::time::Duration::ZERO;
        let client = bunko_mega::Client::login(cfg, "r@cloudsim.test", "pw", None)
            .await
            .unwrap();
        client.session().unwrap().sid().to_string()
    };
    let c = http();
    let cs = |body: Value| {
        let c = c.clone();
        let url = format!("{}cs?id=1&sid={sid}", sim.mega_origin());
        async move {
            c.post(url)
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let v = cs(json!([{"a": "u", "ssl": 0, "s": 4, "ms": 0, "r": 0, "e": 0, "v": 2}])).await;
    let p = v[0]["p"].as_str().unwrap().to_string();
    let raw = c
        .post(format!("{p}/0"))
        .body(b"abcd".to_vec())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        !raw.iter().all(|b| b.is_ascii_alphanumeric()),
        "binary: {raw:?}"
    );
    let h = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
    let v = cs(json!([{"a": "p", "t": bunko_cloudsim::mega::ROOT,
        "n": [{"h": h, "t": 0, "a": "AAAAAAAAAAAAAAAAAAAAAA", "k": "AAAAAAAAAAAAAAAAAAAAAA"}]}]))
    .await;
    assert!(v[0]["f"][0]["h"].is_string(), "{v}");
    sim.assert_clean();
}
