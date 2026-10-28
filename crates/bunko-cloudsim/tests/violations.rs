//! A request the simulator cannot accept is a 400 plus a violation, never a panic.

#[tokio::test]
async fn an_unknown_q_clause_is_a_400_and_one_violation() {
    let sim = bunko_cloudsim::start().await;
    sim.drive("default").mkdir("root", "mokuro-reader");
    let http = reqwest::Client::new();
    let r = http
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth("tok")
        .query(&[("q", "name contains 'mokuro'")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["errors"][0]["reason"], "badRequest");
    let v = sim.take_violations();
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(
        v[0].starts_with("google GET /drive/v3/files: unsupported q clause"),
        "{}",
        v[0]
    );
    assert!(v[0].contains("name contains 'mokuro'"), "{}", v[0]);

    // The server is still up and answers a good request.
    let r = http
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth("tok")
        .query(&[("q", "'me' in owners and trashed=false")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    sim.assert_clean();
}

#[tokio::test]
async fn a_malformed_graph_body_and_an_unknown_path_are_violations() {
    let sim = bunko_cloudsim::start().await;
    sim.onedrive("default");
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{}/me/drive/root/children", sim.graph_api_base()))
        .bearer_auth("tok")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = http
        .get(format!("{}/nowhere", sim.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let v = sim.take_violations();
    assert_eq!(v.len(), 2, "{v:?}");
    assert!(v[0].starts_with("microsoft POST /me/drive/root/children: body is not JSON"));
    assert!(v[1].starts_with("GET /nowhere"));
}

#[tokio::test]
async fn a_bad_token_is_a_401_not_a_violation() {
    let sim = bunko_cloudsim::start().await;
    sim.drive("default");
    let r = reqwest::Client::new()
        .get(format!("{}/drive/v3/about", sim.drive_api_base()))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(sim.violations().is_empty());
}

/// `strict_bunko` (bunko-remote's tests turn it on) makes what Bunko's drivers must
/// always send a check on every request again: `keepRevisionForever=false` on a Drive
/// upload PATCH and `$top` on a OneDrive children listing. Off (the reader), both are
/// optional, as they are for the real services.
#[tokio::test]
async fn strict_bunko_checks_what_bunko_must_send_on_every_request() {
    let sim = bunko_cloudsim::start().await;
    let d = sim.drive("default");
    let f = d.put("root", "a.json", b"{}");
    let od = sim.onedrive("default");
    let folder = od.mkdir(bunko_cloudsim::onedrive::ROOT, "mokuro-reader");
    let http = reqwest::Client::new();
    let patch = || {
        http.patch(format!(
            "{}/upload/drive/v3/files/{f}",
            sim.drive_api_base()
        ))
        .query(&[("uploadType", "resumable")])
        .bearer_auth("tok")
        .json(&serde_json::json!({}))
        .send()
    };
    let children = || {
        http.get(format!(
            "{}/me/drive/items/{folder}/children",
            sim.graph_api_base()
        ))
        .bearer_auth("tok")
        .send()
    };
    assert_eq!(patch().await.unwrap().status(), 200);
    assert_eq!(children().await.unwrap().status(), 200);
    assert!(sim.violations().is_empty());

    d.lock().strict_bunko = true;
    od.lock().strict_bunko = true;
    assert_eq!(patch().await.unwrap().status(), 400);
    assert_eq!(children().await.unwrap().status(), 400);
    let v = sim.take_violations();
    assert_eq!(v.len(), 2, "{v:?}");
    assert!(v[0].contains("keepRevisionForever=false"), "{}", v[0]);
    assert!(v[1].contains("$top"), "{}", v[1]);
    // The knob is on the control API too.
    let r = http
        .post(format!("{}/_sim/knobs", sim.base))
        .json(&serde_json::json!({"provider": "google", "strict_bunko": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(patch().await.unwrap().status(), 200);
}
