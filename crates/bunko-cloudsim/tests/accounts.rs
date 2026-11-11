//! Accounts and tokens: isolation between accounts, revoke and expire, the OAuth
//! refresh endpoints, and two clients sharing one account.

use bunko_cloudsim::{CloudSim, Provider};
use serde_json::Value;

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn list_names(sim: &CloudSim, token: &str) -> (u16, Vec<String>) {
    let r = http()
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth(token)
        .query(&[("q", "'me' in owners and trashed=false")])
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    if status != 200 {
        return (status, Vec::new());
    }
    let v: Value = r.json().await.unwrap();
    let mut names: Vec<String> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    (status, names)
}

async fn refresh(sim: &CloudSim, p: Provider, rt: &str) -> (u16, Value) {
    let r = http()
        .post(sim.token_url(p))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("client_id", "test-client"),
        ])
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn two_accounts_are_isolated() {
    let sim = bunko_cloudsim::start().await;
    let a = sim.token(Provider::Google, "alice");
    let b = sim.token(Provider::Google, "bob");
    sim.drive("alice").put("root", "alice.cbz", b"a");
    sim.drive("bob").put("root", "bob.cbz", b"b");
    assert_eq!(
        list_names(&sim, &a.access_token).await,
        (200, vec!["alice.cbz".into()])
    );
    assert_eq!(
        list_names(&sim, &b.access_token).await,
        (200, vec!["bob.cbz".into()])
    );

    // A file id from one account is not found with the other's token.
    let id = sim.drive("alice").put("root", "secret.cbz", b"s");
    let r = http()
        .get(format!("{}/drive/v3/files/{id}", sim.drive_api_base()))
        .bearer_auth(&b.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // OneDrive too; and a Google token is no Microsoft token.
    let ma = sim.token(Provider::Microsoft, "alice");
    let mb = sim.token(Provider::Microsoft, "bob");
    sim.onedrive("alice")
        .put(bunko_cloudsim::onedrive::ROOT, "mine.cbz", b"m");
    let get = |t: String| {
        let url = format!("{}/me/drive/root:/mine.cbz", sim.graph_api_base());
        async move {
            http()
                .get(url)
                .bearer_auth(t)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(get(ma.access_token.clone()).await, 200);
    assert_eq!(get(mb.access_token.clone()).await, 404);
    assert_eq!(get(a.access_token.clone()).await, 401);
}

#[tokio::test]
async fn revoke_kills_the_grant() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Google, "alice");
    assert_eq!(list_names(&sim, &t.access_token).await.0, 200);
    assert!(sim.revoke(&t.access_token));
    assert_eq!(list_names(&sim, &t.access_token).await.0, 401);
    let (s, v) = refresh(&sim, Provider::Google, &t.refresh_token).await;
    assert_eq!(s, 400);
    assert_eq!(v["error"], "invalid_grant");

    // Revoking by the refresh token works the same way, on Microsoft too.
    let m = sim.token(Provider::Microsoft, "alice");
    assert!(sim.revoke(&m.refresh_token));
    let (s, v) = refresh(&sim, Provider::Microsoft, &m.refresh_token).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
    let r = http()
        .get(format!("{}/me/drive", sim.graph_api_base()))
        .bearer_auth(&m.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(!sim.revoke("never-issued"));
    // Another provider's refresh token is no grant here.
    let g = sim.token(Provider::Google, "carol");
    let (s, v) = refresh(&sim, Provider::Microsoft, &g.refresh_token).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
}

#[tokio::test]
async fn an_expired_token_401s_and_its_refresh_token_still_works() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Google, "alice");
    sim.drive("alice").put("root", "v.cbz", b"v");
    assert!(sim.expire(&t.access_token));
    assert_eq!(list_names(&sim, &t.access_token).await.0, 401);
    let (s, v) = refresh(&sim, Provider::Google, &t.refresh_token).await;
    assert_eq!(s, 200, "{v}");
    let fresh = v["access_token"].as_str().unwrap();
    assert_ne!(fresh, t.access_token);
    assert_eq!(v["expires_in"], 3600);
    // Google keeps the refresh token: none in the answer.
    assert!(v.get("refresh_token").is_none());
    assert_eq!(list_names(&sim, fresh).await, (200, vec!["v.cbz".into()]));
    assert_eq!(sim.refreshes(), 1);
}

#[tokio::test]
async fn microsoft_rotates_the_refresh_token() {
    let sim = bunko_cloudsim::start().await;
    let t = sim.token(Provider::Microsoft, "alice");
    let (s, v) = refresh(&sim, Provider::Microsoft, &t.refresh_token).await;
    assert_eq!(s, 200);
    let rotated = v["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(rotated, t.refresh_token);
    let (s, v2) = refresh(&sim, Provider::Microsoft, &rotated).await;
    assert_eq!(s, 200);
    assert_ne!(v2["refresh_token"].as_str().unwrap(), rotated);
    let r = http()
        .get(format!("{}/me/drive", sim.graph_api_base()))
        .bearer_auth(v2["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_bad_refresh_request_is_a_violation() {
    let sim = bunko_cloudsim::start().await;
    let r = http()
        .post(sim.token_url(Provider::Google))
        .form(&[("grant_type", "authorization_code"), ("code", "x")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v = sim.take_violations();
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("grant_type"), "{}", v[0]);
}

/// One resumable upload through the session protocol, in `chunk`-byte PUTs.
async fn resumable(api: String, token: String, name: String, data: Vec<u8>, chunk: usize) {
    let c = http();
    let r = c
        .post(format!("{api}/upload/drive/v3/files"))
        .query(&[("uploadType", "resumable")])
        .bearer_auth(&token)
        .header("X-Upload-Content-Length", data.len().to_string())
        .json(&serde_json::json!({"name": name, "parents": ["root"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let mut at = 0;
    while at < data.len() {
        let end = (at + chunk).min(data.len());
        let r = c
            .put(&loc)
            .bearer_auth(&token)
            .header(
                "Content-Range",
                format!("bytes {at}-{}/{}", end - 1, data.len()),
            )
            .body(data[at..end].to_vec())
            .send()
            .await
            .unwrap();
        let want = if end == data.len() { 200 } else { 308 };
        assert_eq!(r.status().as_u16(), want);
        at = end;
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_upload_to_one_account_at_once() {
    let sim = bunko_cloudsim::start().await;
    // Two grants on one account, as Bunko and the reader would hold.
    let bunko = sim.token(Provider::Google, "shared");
    let reader = sim.token(Provider::Google, "shared");
    let a: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
    let b: Vec<u8> = (0..(1 << 20) + 17)
        .map(|i| (i % 241) as u8 ^ 0x5a)
        .collect();
    let api = sim.drive_api_base();
    let t1 = tokio::spawn(resumable(
        api.clone(),
        bunko.access_token,
        "a.cbz".into(),
        a.clone(),
        64 << 10,
    ));
    let t2 = tokio::spawn(resumable(
        api,
        reader.access_token,
        "b.cbz".into(),
        b.clone(),
        48 << 10,
    ));
    t1.await.unwrap();
    t2.await.unwrap();
    let files = sim.drive("shared").lock().files();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].0, "a.cbz");
    assert_eq!(files[0].1.as_ref(), a.as_slice());
    assert_eq!(files[1].0, "b.cbz");
    assert_eq!(files[1].1.as_ref(), b.as_slice());
}

/// The fixed token the mock tests have always used opens the `default` account only,
/// never another one (which used to depend on HashMap order).
#[tokio::test]
async fn the_fixed_token_opens_only_the_default_account() {
    let sim = bunko_cloudsim::start().await;
    // Without a `default` account, `tok` opens nothing.
    sim.drive("alice").put("root", "alice.cbz", b"a");
    sim.onedrive("alice")
        .put(bunko_cloudsim::onedrive::ROOT, "alice.cbz", b"a");
    assert_eq!(list_names(&sim, "tok").await.0, 401);
    let graph = |t: &'static str| {
        let url = format!("{}/me/drive/root:/alice.cbz", sim.graph_api_base());
        async move {
            http()
                .get(url)
                .bearer_auth(t)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(graph("tok").await, 401);
    // With one (and more accounts around), always `default`.
    for n in 0..8 {
        sim.drive(&format!("other{n}")).put("root", "x.cbz", b"x");
    }
    sim.drive("default").put("root", "mine.cbz", b"m");
    for _ in 0..10 {
        assert_eq!(
            list_names(&sim, "tok").await,
            (200, vec!["mine.cbz".into()])
        );
    }
    sim.onedrive("default");
    assert_eq!(graph("tok").await, 404, "default's drive has no alice.cbz");
}
