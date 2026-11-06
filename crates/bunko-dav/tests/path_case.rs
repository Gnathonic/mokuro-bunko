//! Library paths are case-insensitive and case-preserving on every host, as on NTFS (0.5.3
//! `tests/integration/test_path_case.py`: `TestCanonicalizer`, `TestCaseInsensitiveHost`,
//! and the DAV half of `TestCaseOnlyRenames`; the full-stack half, with auth and the
//! database, is `bunko-server/tests/dav_path_case.rs`).
//!
//! A library on a case-sensitive filesystem can end up with `Kingdom/` and a stray `kingdom/`
//! (an upload spelled in another case), and the catalog then showed only the stray folder's
//! one volume. Every host now behaves as NTFS does: that request lands in `Kingdom/`. The
//! tests give the same results on case-sensitive and case-insensitive filesystems; a
//! fixture that needs two spellings creates the winner (the byte-smallest) first.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use axum::body::Body;
use bunko_dav::LibraryPathCanonicalizer;
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

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

// --- TestCanonicalizer --------------------------------------------------------------

fn kingdom_library() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().to_path_buf();
    std::fs::create_dir(lib.join("Kingdom")).unwrap();
    std::fs::write(lib.join("Kingdom/第01巻.cbz"), b"").unwrap();
    (dir, lib)
}

/// Every host resolves the same way: no probe, the listing decides.
fn canonicalizer() -> (tempfile::TempDir, LibraryPathCanonicalizer) {
    let (dir, lib) = kingdom_library();
    (dir, LibraryPathCanonicalizer::new(lib))
}

#[test]
fn existing_segments_take_their_on_disk_spelling() {
    let (_dir, c) = canonicalizer();
    assert_eq!(
        c.canonicalize("kingdom/第01巻.CBZ", None),
        "Kingdom/第01巻.cbz"
    );
    assert_eq!(c.canonicalize("KINGDOM/", None), "Kingdom/");
    assert_eq!(
        c.canonicalize("kingdom/New Volume.cbz", None),
        "Kingdom/New Volume.cbz"
    );
    assert_eq!(c.canonicalize("Other/kingdom", None), "Other/kingdom");
}

#[test]
fn a_decomposed_spelling_matches_the_composed_folder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Pok\u{e9}mon")).unwrap(); // composed é
    let c = LibraryPathCanonicalizer::new(dir.path());
    let decomposed = "POKE\u{301}MON/v1.cbz";
    assert_eq!(c.canonicalize(decomposed, None), "Pok\u{e9}mon/v1.cbz");
}

/// Every spelling, the exact one of another entry included, reaches one entry: the
/// byte-smallest. It is created first, so a host that keeps one folder (APFS, NTFS,
/// casefold) and one that keeps two (ext4) give the same answer.
#[test]
fn every_spelling_reaches_one_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Kingdom")).unwrap();
    let _ = std::fs::create_dir(dir.path().join("kingdom"));
    let c = LibraryPathCanonicalizer::new(dir.path());
    for spelling in ["kingdom/x", "Kingdom/x", "KINGDOM/x"] {
        assert_eq!(c.canonicalize(spelling, None), "Kingdom/x", "{spelling}");
    }
}

#[test]
fn a_rename_of_the_source_keeps_the_requested_spelling() {
    let (_dir, c) = canonicalizer();
    assert_eq!(c.canonicalize("KINGDOM/", Some("Kingdom")), "KINGDOM/");
    assert_eq!(
        c.canonicalize("kingdom/第01巻.CBZ", Some("Kingdom/第01巻.cbz")),
        "Kingdom/第01巻.CBZ"
    );
    // Not the source: resolved as usual.
    assert_eq!(c.canonicalize("KINGDOM/", Some("Other")), "Kingdom/");
}

#[test]
fn traversal_is_never_resolved() {
    let (_dir, c) = canonicalizer();
    assert_eq!(
        c.canonicalize("kingdom/../KINGDOM", None),
        "kingdom/../KINGDOM"
    );
}

#[test]
fn the_request_rewrite_keeps_the_query_and_the_destination_shape() {
    let env = Env::new();
    let (mut parts, _) = request(
        "MOVE",
        "/mokuro-reader/SERIES/vol1.cbz?x=1",
        &[("destination", &dest("/mokuro-reader/Series/VOL1.cbz?y#z"))],
        b"",
    )
    .into_parts();
    env.dav.path_case().rewrite_request(&mut parts);
    assert_eq!(parts.uri.to_string(), "/mokuro-reader/series/vol1.cbz?x=1");
    assert_eq!(
        parts.headers["destination"],
        "http://localhost:8080/mokuro-reader/series/VOL1.cbz?y#z"
    );
    // Per-user files and the roots are never resolved against the library.
    for path in [
        "/mokuro-reader/volume-data.json",
        "/mokuro-reader/",
        "/MOKURO-READER/series",
    ] {
        let (mut parts, _) = request("GET", path, &[], b"").into_parts();
        env.dav.path_case().rewrite_request(&mut parts);
        assert_eq!(parts.uri.path(), path);
    }
    // A per-user name in another case takes its canonical spelling.
    let (mut parts, _) = request("GET", "/mokuro-reader/Volume-Data.json", &[], b"").into_parts();
    assert!(bunko_dav::PathCase::may_rewrite(&parts));
    env.dav.path_case().rewrite_request(&mut parts);
    assert_eq!(parts.uri.path(), "/mokuro-reader/volume-data.json");
}

// --- TestCaseInsensitiveHost: NTFS/APFS, where the filesystem itself opens `Kingdom` as
// `kingdom` ---------------------------------------------------------------------------

/// WebDAV deletes an existing MOVE destination first (`Overwrite: T`); on a
/// case-insensitive host that destination IS the source. A hard link reproduces "two
/// spellings, one file" on a case-sensitive test host; elsewhere the filesystem does.
/// The source is spelled `Vol9.cbz`, the byte-smallest, so it is the entry every
/// spelling reaches on both kinds of host.
#[tokio::test]
async fn a_moves_case_variant_destination_is_not_an_existing_resource() {
    let env = Env::new();
    let series = env.lib("series");
    std::fs::write(series.join("Vol9.cbz"), b"volume 9").unwrap();
    std::fs::write(series.join("Vol9.mokuro"), b"{}").unwrap();
    if !series.join("vol9.cbz").exists() {
        std::fs::hard_link(series.join("Vol9.cbz"), series.join("vol9.cbz")).unwrap();
    }

    // Not a MOVE: the other spelling reaches the volume.
    let r = env
        .req(None, "GET", "/mokuro-reader/series/vol9.cbz", &[], b"")
        .await;
    assert_eq!(r.code(), 200);

    let d = dest("/mokuro-reader/series/vol9.cbz");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/series/Vol9.cbz",
        &[("destination", &d), ("overwrite", "T")],
        b"",
    )
    .await;
    assert_eq!(r.code(), 204, "{}", r.text());
    // Reported as existing, the destination would have been deleted first -- with the
    // source volume's sidecars and records.
    assert!(
        !env.hooks.audits().iter().any(|a| a.action == "delete"),
        "{:?}",
        env.hooks.calls()
    );
    assert!(!env.hooks.has("forget_volume_upload"));
    assert!(
        env.hooks
            .has("rename_volume_upload series/Vol9.cbz series/vol9.cbz")
    );
    let listed = names(&series);
    assert!(listed.contains(&"vol9.cbz".to_string()), "{listed:?}");
    assert!(!listed.contains(&"Vol9.cbz".to_string()), "{listed:?}");
    assert!(
        listed.iter().any(|n| n.eq_ignore_ascii_case("vol9.mokuro")),
        "{listed:?}"
    );
    assert_eq!(std::fs::read(series.join("vol9.cbz")).unwrap(), b"volume 9");
}

#[tokio::test]
async fn a_case_fix_destination_keeps_its_spelling() {
    let env = Env::new();
    let library = env.lib("").canonicalize().unwrap();
    assert_eq!(
        env.dav.destination_physical("/mokuro-reader/Series/"),
        Some(library.join("Series"))
    );
    assert_eq!(
        env.dav
            .destination_physical("/mokuro-reader/series/VOL1.cbz"),
        Some(library.join("series").join("VOL1.cbz"))
    );
    assert_eq!(
        env.dav.destination_physical("/mokuro-reader/../escape"),
        None
    );
    assert_eq!(
        env.dav.destination_physical("/mokuro-reader/series/.."),
        None
    );
}

/// The last segment is not resolved (see above), but a COPY writes THROUGH a symlink at
/// the destination: where it leads must still be inside the library.
#[cfg(unix)]
#[tokio::test]
async fn a_destination_symlink_leading_outside_is_refused() {
    let env = Env::new();
    let outside = env.dir.path().join("outside.cbz");
    std::fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, env.lib("series/link.cbz")).unwrap();
    assert_eq!(
        env.dav
            .destination_physical("/mokuro-reader/series/link.cbz"),
        None
    );

    let d = dest("/mokuro-reader/series/link.cbz");
    let r = req(
        &env,
        Some("admin"),
        "COPY",
        "/mokuro-reader/manga1.cbz",
        &[("destination", &d), ("overwrite", "T")],
        b"",
    )
    .await;
    // 0.5.3 reported the refused copy as done (WsgiDAV ignores `copy_move_single`'s
    // False); refused outright here. What matters: nothing written through the link.
    assert_eq!(r.code(), 403);
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
    assert!(
        std::fs::symlink_metadata(env.lib("series/link.cbz"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

// --- requests reach the existing spelling (DAV level) ---------------------------------

#[tokio::test]
async fn get_and_propfind_find_a_case_variant() {
    let env = Env::new();
    let r = req(
        &env,
        None,
        "GET",
        "/mokuro-reader/Series/VOL1.cbz",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.code(), 200);
    assert_eq!(r.body, b"volume 1");

    let listing = req(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/SERIES/",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(listing.code(), 207);
    // The hrefs carry the on-disk spelling.
    assert!(listing.text().contains("/mokuro-reader/series/vol1.cbz"));
    assert!(!listing.text().contains("SERIES"));
}

#[tokio::test]
async fn mkcol_and_put_of_a_case_variant_reach_the_existing_folder() {
    let env = Env::new();
    assert_eq!(
        req(
            &env,
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/SERIES",
            &[],
            b""
        )
        .await
        .code(),
        405
    );
    let r = req(
        &env,
        Some("uploader"),
        "PUT",
        "/mokuro-reader/SERIES/vol2.cbz",
        &[],
        &cbz_bytes(1),
    )
    .await;
    assert!(matches!(r.code(), 200 | 201 | 204), "{}", r.text());
    assert!(env.lib("series/vol2.cbz").is_file());
    assert!(!names(&env.lib("")).contains(&"SERIES".to_string()));
}

/// The reader's order: make the series folder (a 405 means it exists), then upload into
/// it.
#[tokio::test]
async fn a_new_folder_keeps_the_spelling_it_was_created_with() {
    let env = Env::new();
    let mkcol = |path: &'static str| req(&env, Some("uploader"), "MKCOL", path, &[], b"");
    assert_eq!(mkcol("/mokuro-reader/Kingdom").await.code(), 201);
    assert_eq!(mkcol("/mokuro-reader/kingdom").await.code(), 405);
    for path in [
        "/mokuro-reader/Kingdom/v01.cbz",
        "/mokuro-reader/kingdom/v13.cbz",
    ] {
        let r = req(&env, Some("uploader"), "PUT", path, &[], &cbz_bytes(1)).await;
        assert!(matches!(r.code(), 200 | 201 | 204), "{path}: {}", r.text());
    }
    assert_eq!(names(&env.lib("Kingdom")), ["v01.cbz", "v13.cbz"]);
    assert!(!names(&env.lib("")).contains(&"kingdom".to_string()));
}

/// A library that went through 0.5.3's case merge: `Kingdom/` holds every volume, and a reader
/// still asks for `kingdom/...`.
#[tokio::test]
async fn the_kingdom_scenario() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Kingdom")).unwrap();
    for v in 1..=13 {
        std::fs::write(env.lib(&format!("Kingdom/第{v:02}巻.cbz")), b"cbz").unwrap();
    }
    std::fs::write(
        env.lib("Kingdom/第13巻.mokuro"),
        b"{\"title\": \"Kingdom\"}",
    )
    .unwrap();
    let r = req(
        &env,
        None,
        "GET",
        "/mokuro-reader/kingdom/%E7%AC%AC13%E5%B7%BB.mokuro",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.code(), 200);
    assert_eq!(r.body, b"{\"title\": \"Kingdom\"}");
    let listing = req(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/kingdom",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(listing.code(), 207);
    let text = listing.text();
    // The folder and its 14 files, every href in the on-disk spelling.
    assert_eq!(
        text.matches("/mokuro-reader/Kingdom/").count(),
        15,
        "{text}"
    );
    assert!(!text.contains("/mokuro-reader/kingdom"));
    assert!(text.contains("/mokuro-reader/Kingdom/%E7%AC%AC13%E5%B7%BB.mokuro"));
}

// --- case-only renames (DAV level) -----------------------------------------------------

#[tokio::test]
async fn renaming_a_folder_to_fix_its_case() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol1.mokuro"), b"{}").unwrap();
    let d = dest("/mokuro-reader/Series");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/series",
        &[
            ("destination", &d),
            ("overwrite", "T"),
            ("depth", "infinity"),
        ],
        b"",
    )
    .await;
    // 0.5.2 answered 423: the case-folded write locks conflicted with themselves.
    assert!(matches!(r.code(), 201 | 204), "{} {}", r.code(), r.text());
    let top = names(&env.lib(""));
    assert!(top.contains(&"Series".to_string()));
    assert!(!top.contains(&"series".to_string()));
    assert_eq!(names(&env.lib("Series")), ["vol1.cbz", "vol1.mokuro"]);
    assert!(
        env.hooks
            .has("rename_volume_upload series/vol1.cbz Series/vol1.cbz")
    );
    assert_eq!(env.dav.write_locks().held_count(), 0);
}

#[tokio::test]
async fn renaming_a_file_to_fix_its_case() {
    let env = Env::new();
    let d = dest("/mokuro-reader/series/Vol1.cbz");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/series/vol1.cbz",
        &[("destination", &d), ("overwrite", "F")],
        b"",
    )
    .await;
    assert!(matches!(r.code(), 201 | 204), "{} {}", r.code(), r.text());
    assert_eq!(names(&env.lib("series")), ["Vol1.cbz"]);
}

#[tokio::test]
async fn a_case_fix_through_a_differently_spelled_parent() {
    let env = Env::new();
    let d = dest("/mokuro-reader/Series/VOL1.cbz");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/SERIES/vol1.cbz",
        &[("destination", &d), ("overwrite", "F")],
        b"",
    )
    .await;
    assert!(matches!(r.code(), 201 | 204), "{} {}", r.code(), r.text());
    assert_eq!(
        names(&env.lib(""))
            .iter()
            .filter(|n| *n == "series")
            .count(),
        1
    );
    assert_eq!(names(&env.lib("series")), ["VOL1.cbz"]);
}

#[tokio::test]
async fn moving_onto_a_case_variant_of_another_file_is_moving_onto_it() {
    let env = Env::new();
    let d = dest("/mokuro-reader/MANGA2.cbz");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/manga1.cbz",
        &[("destination", &d), ("overwrite", "F")],
        b"",
    )
    .await;
    assert_eq!(r.code(), 412);
    assert_eq!(
        std::fs::read(env.lib("manga2.cbz")).unwrap(),
        b"fake cbz content 2"
    );
}

#[tokio::test]
async fn copying_onto_its_own_case_variant_is_refused() {
    let env = Env::new();
    let d = dest("/mokuro-reader/MANGA1.cbz");
    let r = req(
        &env,
        Some("admin"),
        "COPY",
        "/mokuro-reader/manga1.cbz",
        &[("destination", &d), ("overwrite", "T")],
        b"",
    )
    .await;
    assert_eq!(r.code(), 403);
    assert!(!names(&env.lib("")).contains(&"MANGA1.cbz".to_string()));
}

/// A case-fixing MOVE onto a name another entry already holds in another spelling
/// (only a case-sensitive disk can) is refused: `Overwrite: T` used to delete the other
/// spelling's folder first. Whatever the host, nothing is deleted or overwritten.
#[tokio::test]
async fn a_case_fix_onto_an_existing_other_spelling_is_refused() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Kingdom")).unwrap();
    std::fs::write(env.lib("Kingdom/a.cbz"), b"a").unwrap();
    let _ = std::fs::create_dir(env.lib("kingdom"));
    std::fs::write(env.lib("kingdom/b.cbz"), b"b").unwrap();
    let two = holds_two_spellings(&env.lib(""), "Kingdom", "kingdom");

    let d = dest("/mokuro-reader/kingdom");
    let r = req(
        &env,
        Some("admin"),
        "MOVE",
        "/mokuro-reader/Kingdom",
        &[
            ("destination", &d),
            ("overwrite", "T"),
            ("depth", "infinity"),
        ],
        b"",
    )
    .await;
    // Every fixture file is still there with its bytes, under one spelling or another.
    let all: Vec<(String, Vec<u8>)> = names(&env.lib(""))
        .into_iter()
        .filter(|n| n.eq_ignore_ascii_case("kingdom"))
        .flat_map(|folder| {
            let dir = env.lib(&folder);
            names(&dir)
                .into_iter()
                .map(move |n| (n.clone(), std::fs::read(dir.join(&n)).unwrap()))
        })
        .collect();
    assert!(all.contains(&("a.cbz".into(), b"a".to_vec())), "{all:?}");
    assert!(all.contains(&("b.cbz".into(), b"b".to_vec())), "{all:?}");
    assert!(
        !env.hooks.audits().iter().any(|a| a.action == "delete"),
        "{:?}",
        env.hooks.calls()
    );
    assert!(
        !env.hooks.calls().iter().any(|c| c.starts_with("forget_")),
        "{:?}",
        env.hooks.calls()
    );
    if two {
        assert_eq!(r.code(), 409, "{}", r.text());
        assert!(holds_two_spellings(&env.lib(""), "Kingdom", "kingdom"));
    }
}

/// A listing shows one spelling of a name: the one every request reaches. Another
/// spelling (a case-sensitive disk only) is shadowed. Written winner first, so every
/// host lists the same.
#[tokio::test]
async fn a_listing_shows_one_spelling_of_a_name() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Kingdom")).unwrap();
    std::fs::write(env.lib("Kingdom/a.cbz"), b"a").unwrap();
    let _ = std::fs::create_dir(env.lib("kingdom"));
    std::fs::write(env.lib("kingdom/b.cbz"), b"b").unwrap();
    let listing = req(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(listing.code(), 207);
    let text = listing.text();
    assert!(text.contains("/mokuro-reader/Kingdom/"), "{text}");
    assert!(!text.contains("/mokuro-reader/kingdom/"), "{text}");
}

// --- The per-user names are reserved in every spelling (0.8) ------------------------

/// Inside the caller's own history tree nothing resolves against the library; an
/// existing device folder is reached in any spelling (one folder per device on every
/// host).
#[tokio::test]
async fn history_paths_never_resolve_against_the_library() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("Kingdom/Dev")).unwrap();
    let pc = env.dav.path_case();
    assert_eq!(
        pc.canonical_path("/mokuro-reader/history/Dev/x", None),
        "/mokuro-reader/history/Dev/x"
    );
    assert_eq!(
        pc.canonical_path("/mokuro-reader/history/dev/x", None),
        "/mokuro-reader/history/dev/x"
    );

    for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev"] {
        let r = req(&env, Some("reader"), "MKCOL", path, &[], b"").await;
        assert_eq!(r.code(), 201, "MKCOL {path}: {}", r.text());
    }
    let r = req(
        &env,
        Some("reader"),
        "PUT",
        "/mokuro-reader/history/DEV/x.events",
        &[],
        b"events",
    )
    .await;
    assert!(matches!(r.code(), 201 | 204), "{}", r.text());
    let history = env.base.join("users/reader/history");
    assert_eq!(names(&history), ["dev"]);
    assert_eq!(names(&history.join("dev")), ["x.events"]);
}

/// `/mokuro-reader/` is one case-insensitive namespace: `HISTORY/` and `Goals.json` ARE
/// the caller's `history/` and `goals.json`, whatever the library holds, and never a
/// library path. A legacy library folder spelled like one is hidden, never written.
#[tokio::test]
async fn a_reserved_name_in_any_spelling_is_the_per_user_entry() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("history/dev")).unwrap();
    std::fs::write(env.lib("goals.json"), b"{}").unwrap();
    let pc = env.dav.path_case();
    assert_eq!(
        pc.canonical_path("/mokuro-reader/HISTORY/v.cbz", None),
        "/mokuro-reader/history/v.cbz"
    );
    assert_eq!(
        pc.canonical_path("/mokuro-reader/Goals.json", None),
        "/mokuro-reader/goals.json"
    );

    // A library holding only a legacy `History/` series.
    let env = Env::new();
    std::fs::create_dir_all(env.lib("History")).unwrap();
    std::fs::write(env.lib("History/old.cbz"), b"old").unwrap();
    let pc = env.dav.path_case();
    for spelling in ["HISTORY", "hIstory", "History"] {
        let path = format!("/mokuro-reader/{spelling}/v.cbz");
        assert_eq!(
            pc.canonical_path(&path, None),
            "/mokuro-reader/history/v.cbz"
        );
    }
    let library_before = names(&env.lib(""));
    let body = cbz_bytes(1);
    let put = |path: &'static str| req(&env, Some("uploader"), "PUT", path, &[], &body);
    let canonical = put("/mokuro-reader/history/v.cbz").await.code();
    let other = put("/mokuro-reader/HISTORY/v.cbz").await.code();
    assert_eq!(other, canonical);
    assert!(!matches!(other, 200..=299), "{other}");
    // Nothing written: not in the library's `History/`, not in the account's tree.
    assert_eq!(names(&env.lib("")), library_before);
    assert_eq!(names(&env.lib("History")), ["old.cbz"]);
    assert!(!env.base.join("users/uploader/history").exists());

    let listing = req(
        &env,
        Some("uploader"),
        "PROPFIND",
        "/mokuro-reader/",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(listing.code(), 207);
    assert!(!listing.text().contains("History"), "{}", listing.text());
}

/// `Thumbnails` is a series like any, on every store: no cover folder is made in the
/// library (a second spelling of it), and a request for it reaches the series.
#[tokio::test]
async fn a_series_named_thumbnails_is_a_series() {
    let env = Env::new();
    let r = req(
        &env,
        Some("admin"),
        "MKCOL",
        "/mokuro-reader/Thumbnails",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    let r = req(
        &env,
        Some("admin"),
        "PUT",
        "/mokuro-reader/thumbnails/v.cbz",
        &[],
        &cbz_bytes(1),
    )
    .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    // A restart makes nothing beside it.
    let again = Env::with_base(tempfile::tempdir().unwrap(), env.base.clone());
    drop(again);
    let spellings: Vec<String> = names(&env.lib(""))
        .into_iter()
        .filter(|n| n.eq_ignore_ascii_case("thumbnails"))
        .collect();
    assert_eq!(spellings, ["Thumbnails"]);
    assert_eq!(names(&env.lib("Thumbnails")), ["v.cbz"]);
}
