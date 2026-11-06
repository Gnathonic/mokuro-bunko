//! One rule for which names exist, on every host: decided from the folder listings,
//! never by opening the requested spelling. A case-sensitive disk (or a remote store)
//! can hold two spellings of a name; the one no request reaches is hidden, and so are
//! the files rule v1 gives to a hidden archive (its sidecars, cover and corrections,
//! named by its exact stem). Nothing hidden is ever shown or read through another
//! spelling, and the whole group is frozen: every write to a member (the winner
//! included), to a file rule v1 gives either, or to a folder holding them, answers 409
//! until the variants are merged by hand. No request makes a second spelling of a
//! name.
//!
//! Fixtures that need two spellings run that part only where the folder really lists
//! both (a case-sensitive disk); everything else asserts one outcome on every host.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use axum::body::Body;
use common::*;
use http::Request;

fn dest(path: &str) -> String {
    format!("http://localhost:8080{path}")
}

/// What the server does with every request: the path-case rewrite, then WebDAV.
async fn send(env: &Env, user: Option<&str>, req: Request<Body>) -> Resp {
    let (mut parts, body) = req.into_parts();
    env.dav.path_case().rewrite_request(&mut parts);
    env.send(user, Request::from_parts(parts, body)).await
}

async fn req(
    env: &Env,
    user: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    send(env, user, request(method, path, headers, body)).await
}

/// The same as the cloud-root owner (its corrections are the physical files).
async fn as_owner(env: &Env, method: &str, path: &str, body: &[u8]) -> Resp {
    let mut ctx = env.ctx(Some("admin"));
    ctx.owns_cloud_root = true;
    let (mut parts, body) = request(method, path, &[], body).into_parts();
    env.dav.path_case().rewrite_request(&mut parts);
    env.send_ctx(ctx, Request::from_parts(parts, body)).await
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// `path` written with `bytes` only when no entry of that name exists in any
/// spelling the host folds (a case-insensitive disk opens the other one).
fn create_new(path: &std::path::Path, bytes: &[u8]) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        f.write_all(bytes).unwrap();
    }
}

/// A correction layer: a JSON object with `pages`.
fn layer(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5",
        "pages": [{"img_path": "001.png", "blocks": [{"lines": [text]}]}],
    }))
    .unwrap()
}

/// `S/` holding `V.cbz` (the winner, the byte-smallest) and, where the disk keeps two
/// spellings, the hidden `v.cbz` with its own OCR, cover and correction. True when the
/// folder lists both archives.
fn hidden_fixture(env: &Env, folder: &str) -> bool {
    let dir = env.lib(folder);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("V.cbz"), b"winner").unwrap();
    create_new(&dir.join("v.cbz"), b"hidden");
    std::fs::write(dir.join("v.mokuro"), b"hidden ocr").unwrap();
    std::fs::write(dir.join("v.webp"), b"hidden cover").unwrap();
    std::fs::write(dir.join("v.edit.mokuro"), layer("hidden edit")).unwrap();
    holds_two_spellings(&dir, "V.cbz", "v.cbz")
}

fn hidden_files_intact(env: &Env, folder: &str) {
    let dir = env.lib(folder);
    assert_eq!(std::fs::read(dir.join("v.cbz")).unwrap(), b"hidden");
    assert_eq!(std::fs::read(dir.join("v.mokuro")).unwrap(), b"hidden ocr");
    assert_eq!(std::fs::read(dir.join("v.webp")).unwrap(), b"hidden cover");
    assert_eq!(
        std::fs::read(dir.join("v.edit.mokuro")).unwrap(),
        layer("hidden edit")
    );
    assert_eq!(
        names(&dir),
        ["V.cbz", "v.cbz", "v.edit.mokuro", "v.mokuro", "v.webp"]
    );
}

/// A hidden archive's own files are no file of the archive that shadows it: not listed,
/// not read, not written, not deleted through any spelling.
#[tokio::test]
async fn a_hidden_archives_files_are_never_reached() {
    let env = Env::new();
    let two = hidden_fixture(&env, "S");
    let listing = req(
        &env,
        Some("admin"),
        "PROPFIND",
        "/mokuro-reader/S/",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(listing.code(), 207);
    let text = listing.text();
    assert!(text.contains("/mokuro-reader/S/V.cbz"), "{text}");
    if !two {
        // One archive, `V.cbz` (an insensitive disk kept one spelling): `v.mokuro`
        // is its OCR, reached in any spelling.
        let got = req(&env, None, "GET", "/mokuro-reader/S/V.mokuro", &[], b"").await;
        assert_eq!((got.code(), got.body), (200, b"hidden ocr".to_vec()));
        return;
    }
    for hidden in ["v.cbz", "v.mokuro", "v.webp", "v.edit.mokuro"] {
        assert!(
            !text.contains(&format!("/mokuro-reader/S/{hidden}")),
            "{text}"
        );
    }
    for name in ["V.mokuro", "v.mokuro", "V.webp", "v.webp"] {
        let path = format!("/mokuro-reader/S/{name}");
        let got = req(&env, Some("admin"), "GET", &path, &[], b"").await;
        assert_eq!(got.code(), 404, "GET {name}");
        let put = req(&env, Some("admin"), "PUT", &path, &[], b"overwritten").await;
        assert_eq!(put.code(), 409, "PUT {name}: {}", put.text());
        let del = req(&env, Some("admin"), "DELETE", &path, &[], b"").await;
        assert_eq!(del.code(), 409, "DELETE {name}: {}", del.text());
    }
    // The cloud-root owner's correction of `V.cbz` is no write into the hidden one's.
    let put = as_owner(
        &env,
        "PUT",
        "/mokuro-reader/S/V.edit.mokuro",
        &layer("owner"),
    )
    .await;
    assert_eq!(put.code(), 409, "{}", put.text());
    let got = as_owner(&env, "GET", "/mokuro-reader/S/V.edit.mokuro", b"").await;
    assert_eq!(got.code(), 404);
    // The winner itself is served as ever.
    let got = req(&env, None, "GET", "/mokuro-reader/S/v.cbz", &[], b"").await;
    assert_eq!((got.code(), got.body), (200, b"winner".to_vec()));
    hidden_files_intact(&env, "S");
}

/// A folder holding a hidden entry is never deleted or replaced whole (DELETE, MOVE or
/// COPY with `Overwrite: T`): 409, and every hidden file stays.
#[tokio::test]
async fn a_folder_holding_a_hidden_entry_is_never_deleted_whole() {
    let env = Env::new();
    let two = hidden_fixture(&env, "S");
    std::fs::create_dir_all(env.lib("A")).unwrap();
    std::fs::write(env.lib("A/V.cbz"), b"a's v").unwrap();
    std::fs::write(env.lib("A/v.mokuro"), b"a's ocr").unwrap();
    let d = dest("/mokuro-reader/S/");
    let headers = [
        ("destination", d.as_str()),
        ("overwrite", "T"),
        ("depth", "infinity"),
    ];
    let copy = req(
        &env,
        Some("admin"),
        "COPY",
        "/mokuro-reader/A/",
        &headers,
        b"",
    )
    .await;
    let moved = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/A/",
        &headers,
        b"",
    )
    .await;
    let delete = req(&env, Some("admin"), "DELETE", "/mokuro-reader/S/", &[], b"").await;
    if !two {
        assert_eq!(copy.code(), 204, "{}", copy.text());
        assert_eq!(moved.code(), 204, "{}", moved.text());
        assert_eq!(delete.code(), 204, "{}", delete.text());
        return;
    }
    assert_eq!(copy.code(), 409, "{}", copy.text());
    assert_eq!(moved.code(), 409, "{}", moved.text());
    assert_eq!(delete.code(), 409, "{}", delete.text());
    hidden_files_intact(&env, "S");
    assert_eq!(names(&env.lib("A")), ["V.cbz", "v.mokuro"]);
    assert!(
        !env.hooks.calls().iter().any(|c| c.starts_with("forget_")),
        "{:?}",
        env.hooks.calls()
    );
}

/// An account's correction of a hidden archive (`edits/S/v.edit.mokuro`, rule v1's for
/// `v.cbz`) is never the reachable `V.cbz`'s: not listed beside it, not read, written or
/// deleted through `V.edit.mokuro`.
#[tokio::test]
async fn a_hidden_archives_correction_stays_its_own() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    std::fs::write(env.lib("S/V.cbz"), b"winner").unwrap();
    create_new(&env.lib("S/v.cbz"), b"hidden");
    let two = holds_two_spellings(&env.lib("S"), "V.cbz", "v.cbz");
    let overlay = env.base.join("users/alice/edits/S/v.edit.mokuro");
    std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
    std::fs::write(&overlay, layer("alice on v")).unwrap();
    let edit = "/mokuro-reader/S/V.edit.mokuro";
    let listing = req(
        &env,
        Some("alice"),
        "PROPFIND",
        "/mokuro-reader/S/",
        &[("depth", "1")],
        b"",
    )
    .await;
    let got = req(&env, Some("alice"), "GET", edit, &[], b"").await;
    if !two {
        // `v.edit.mokuro` is the one archive's correction, in any spelling.
        assert!(listing.text().contains("/mokuro-reader/S/v.edit.mokuro"));
        assert_eq!((got.code(), got.body), (200, layer("alice on v")));
        return;
    }
    assert!(
        !listing.text().contains(".edit.mokuro"),
        "{}",
        listing.text()
    );
    assert_eq!(got.code(), 404);
    let put = req(&env, Some("alice"), "PUT", edit, &[], &layer("alice on V")).await;
    assert_eq!(put.code(), 409, "{}", put.text());
    let del = req(&env, Some("alice"), "DELETE", edit, &[], b"").await;
    assert_eq!(del.code(), 409, "{}", del.text());
    assert_eq!(std::fs::read(&overlay).unwrap(), layer("alice on v"));
    assert_eq!(names(overlay.parent().unwrap()), ["v.edit.mokuro"]);
}

/// Whether the target of a write exists is read from its folder's listing: a request
/// that reaches WebDAV with a spelling the folder lists otherwise (a racing upload made
/// it after PathCase resolved the request; here the request skips the rewrite) gets one
/// answer on every host, and no second spelling is made.
#[tokio::test]
async fn a_name_listed_in_another_spelling_is_one_answer_everywhere() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol 2.cbz"), b"first").unwrap();
    std::fs::write(env.lib("series/w.cbz"), b"w").unwrap();
    std::fs::create_dir(env.lib("series/sub")).unwrap();
    let put = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/series/Vol%202.cbz",
            &[],
            &cbz_bytes(1),
        )
        .await;
    assert_eq!(put.code(), 409, "{}", put.text());
    let to = dest("/mokuro-reader/series/VOL1.cbz");
    let copy = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &to), ("overwrite", "F")],
            b"",
        )
        .await;
    assert_eq!(copy.code(), 409, "{}", copy.text());
    let to = dest("/mokuro-reader/series/W.cbz");
    let moved = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/manga2.cbz",
            &[("destination", &to), ("overwrite", "T")],
            b"",
        )
        .await;
    assert_eq!(moved.code(), 409, "{}", moved.text());
    let mkcol = env
        .req(
            Some("admin"),
            "MKCOL",
            "/mokuro-reader/series/SUB",
            &[],
            b"",
        )
        .await;
    assert_eq!(mkcol.code(), 405, "{}", mkcol.text());
    let get = env
        .req(None, "GET", "/mokuro-reader/series/W.cbz", &[], b"")
        .await;
    assert_eq!(get.code(), 404);
    assert_eq!(
        names(&env.lib("series")),
        ["sub", "vol 2.cbz", "vol1.cbz", "w.cbz"]
    );
    assert_eq!(
        std::fs::read(env.lib("series/vol 2.cbz")).unwrap(),
        b"first"
    );
    assert_eq!(std::fs::read(env.lib("series/w.cbz")).unwrap(), b"w");
    assert!(env.lib("manga2.cbz").is_file());
}

/// Names are one entry when ANY host joins them: Linux casefold's full case folding
/// (`Straße` = `STRASSE`, `ſ` = `s`) included, so every host answers alike.
#[tokio::test]
async fn names_full_case_folding_joins_are_one_entry() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Stra\u{df}e")).unwrap();
    std::fs::write(env.lib("Stra\u{df}e/v.cbz"), b"old").unwrap();
    std::fs::write(env.lib("series/s.cbz"), b"old s").unwrap();
    let mkcol = req(
        &env,
        Some("admin"),
        "MKCOL",
        "/mokuro-reader/STRASSE/",
        &[],
        b"",
    )
    .await;
    assert_eq!(mkcol.code(), 405, "{}", mkcol.text());
    let put = req(
        &env,
        Some("admin"),
        "PUT",
        "/mokuro-reader/STRASSE/v.cbz",
        &[],
        &cbz_bytes(1),
    )
    .await;
    assert_eq!(put.code(), 204, "{}", put.text());
    let put = req(
        &env,
        Some("admin"),
        "PUT",
        "/mokuro-reader/series/%C5%BF.cbz",
        &[],
        &cbz_bytes(1),
    )
    .await;
    assert_eq!(put.code(), 204, "{}", put.text());
    let folders: Vec<String> = names(&env.lib(""))
        .into_iter()
        .filter(|n| n.starts_with("Str") || n.starts_with("STR"))
        .collect();
    assert_eq!(folders, ["Stra\u{df}e"]);
    assert_eq!(names(&env.lib("Stra\u{df}e")), ["v.cbz"]);
    assert_eq!(names(&env.lib("series")), ["s.cbz", "vol1.cbz"]);
    assert_eq!(
        std::fs::read(env.lib("series/s.cbz")).unwrap(),
        cbz_bytes(1)
    );
    let got = req(&env, None, "GET", "/mokuro-reader/strasse/V.CBZ", &[], b"").await;
    assert_eq!(got.code(), 200);
}

/// An account's per-user files are found from the listing of `users/<u>/`: a
/// `Goals.json` made there outside Bunko is its `goals.json` on every host (read, and
/// written under its own spelling, never a second file beside it). The cloud-root
/// owner's `goals.json` is the canonical name in the cloud folder: a listed
/// `Goals.json` is not it, on any host (its writes are refused until it is renamed).
#[tokio::test]
async fn per_user_files_exist_by_the_listing() {
    let env = Env::new();
    let alice = env.base.join("users/alice");
    std::fs::create_dir_all(&alice).unwrap();
    std::fs::write(alice.join("Goals.json"), b"{\"g\":1}").unwrap();
    let got = req(
        &env,
        Some("alice"),
        "GET",
        "/mokuro-reader/goals.json",
        &[],
        b"",
    )
    .await;
    assert_eq!((got.code(), got.body), (200, b"{\"g\":1}".to_vec()));
    let put = req(
        &env,
        Some("alice"),
        "PUT",
        "/mokuro-reader/goals.json",
        &[],
        b"{\"g\":2}",
    )
    .await;
    assert_eq!(put.code(), 204, "{}", put.text());
    assert_eq!(names(&alice), ["Goals.json"]);
    assert_eq!(
        std::fs::read(alice.join("Goals.json")).unwrap(),
        b"{\"g\":2}"
    );
    // The cloud-root owner: the library root lists `Goals.json`, never its
    // `goals.json`.
    std::fs::write(env.lib("Goals.json"), b"{\"owner\":1}").unwrap();
    let got = as_owner(&env, "GET", "/mokuro-reader/goals.json", b"").await;
    assert_eq!(got.code(), 404, "{}", got.text());
    let put = as_owner(&env, "PUT", "/mokuro-reader/goals.json", b"{}").await;
    assert_eq!(put.code(), 409, "{}", put.text());
    assert_eq!(
        std::fs::read(env.lib("Goals.json")).unwrap(),
        b"{\"owner\":1}"
    );
    assert!(
        !names(&env.lib("")).contains(&"goals.json".to_string()),
        "{:?}",
        names(&env.lib(""))
    );
}

/// An account sees its own corrections beside the library files, so a library file
/// spelled like one of them (`v.EDIT.mokuro` beside alice's `V.edit.mokuro`, a name rule
/// v1 does not read as a correction) would be a second spelling in its listings: the
/// write is refused (409), the same on every host, and nothing is made.
#[tokio::test]
async fn a_library_name_an_accounts_correction_spells_otherwise_is_refused() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    std::fs::write(env.lib("S/V.cbz"), b"v").unwrap();
    let put = req(
        &env,
        Some("alice"),
        "PUT",
        "/mokuro-reader/S/V.edit.mokuro",
        &[],
        &layer("alice"),
    )
    .await;
    assert_eq!(put.code(), 201, "{}", put.text());
    let put = req(
        &env,
        Some("alice"),
        "PUT",
        "/mokuro-reader/s/v.EDIT.mokuro",
        &[],
        &layer("again"),
    )
    .await;
    assert_eq!(put.code(), 409, "{}", put.text());
    assert_eq!(names(&env.lib("S")), ["V.cbz"]);
    let listing = req(
        &env,
        Some("alice"),
        "PROPFIND",
        "/mokuro-reader/S/",
        &[("depth", "1")],
        b"",
    )
    .await;
    let text = listing.text();
    assert!(text.contains("/mokuro-reader/S/V.edit.mokuro"), "{text}");
    assert!(!text.contains("EDIT"), "{text}");
}
