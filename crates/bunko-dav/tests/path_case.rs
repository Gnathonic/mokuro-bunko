//! Library paths behave as on NTFS: case-insensitive, case-preserving (0.5.3
//! `tests/integration/test_path_case.py`: `TestCanonicalizer`, `TestCaseInsensitiveHost`,
//! and the DAV half of `TestCaseOnlyRenames`; the full-stack half, with auth and the
//! database, is `bunko-server/tests/dav_path_case.rs`).
//!
//! Prod 2026-10-06: an upload spelled `kingdom/` created a second folder beside `Kingdom/`
//! on the case-sensitive host, and the catalog then showed only the stray folder's one
//! volume. On NTFS that request lands in `Kingdom/`.

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

/// Both a case-sensitive and a case-insensitive host's resolution, over one tree.
fn canonicalizers() -> Vec<(tempfile::TempDir, LibraryPathCanonicalizer)> {
    [true, false]
        .into_iter()
        .map(|sensitive| {
            let (dir, lib) = kingdom_library();
            (dir, LibraryPathCanonicalizer::new(lib, Some(sensitive)))
        })
        .collect()
}

#[test]
fn existing_segments_take_their_on_disk_spelling() {
    for (_dir, c) in canonicalizers() {
        let sensitive = c.case_sensitive();
        assert_eq!(
            c.canonicalize("kingdom/第01巻.CBZ", None),
            "Kingdom/第01巻.cbz",
            "{sensitive}"
        );
        assert_eq!(c.canonicalize("KINGDOM/", None), "Kingdom/", "{sensitive}");
        assert_eq!(
            c.canonicalize("kingdom/New Volume.cbz", None),
            "Kingdom/New Volume.cbz",
            "{sensitive}"
        );
        assert_eq!(
            c.canonicalize("Other/kingdom", None),
            "Other/kingdom",
            "{sensitive}"
        );
    }
}

#[test]
fn a_decomposed_spelling_matches_the_composed_folder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Pok\u{e9}mon")).unwrap(); // composed é
    let c = LibraryPathCanonicalizer::new(dir.path(), Some(true));
    let decomposed = "POKE\u{301}MON/v1.cbz";
    assert_eq!(c.canonicalize(decomposed, None), "Pok\u{e9}mon/v1.cbz");
}

/// A case-sensitive library that already holds both keeps both reachable.
#[test]
fn an_exact_spelling_wins_over_a_variant() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Kingdom")).unwrap();
    if std::fs::create_dir(dir.path().join("kingdom")).is_err() {
        eprintln!("SKIPPED: this filesystem is case-insensitive");
        return;
    }
    let c = LibraryPathCanonicalizer::new(dir.path(), Some(true));
    assert_eq!(c.canonicalize("kingdom/x", None), "kingdom/x");
    assert_eq!(c.canonicalize("Kingdom/x", None), "Kingdom/x");
    assert_eq!(c.canonicalize("KINGDOM/x", None), "Kingdom/x");
}

#[test]
fn a_rename_of_the_source_keeps_the_requested_spelling() {
    for (_dir, c) in canonicalizers() {
        assert_eq!(c.canonicalize("KINGDOM/", Some("Kingdom")), "KINGDOM/");
        assert_eq!(
            c.canonicalize("kingdom/第01巻.CBZ", Some("Kingdom/第01巻.cbz")),
            "Kingdom/第01巻.CBZ"
        );
        // Not the source: resolved as usual.
        assert_eq!(c.canonicalize("KINGDOM/", Some("Other")), "Kingdom/");
    }
}

#[test]
fn traversal_is_never_resolved() {
    for (_dir, c) in canonicalizers() {
        assert_eq!(
            c.canonicalize("kingdom/../KINGDOM", None),
            "kingdom/../KINGDOM"
        );
    }
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
    // Per-user files and the roots are never rewritten.
    for path in [
        "/mokuro-reader/volume-data.json",
        "/mokuro-reader/",
        "/MOKURO-READER/series",
    ] {
        let (mut parts, _) = request("GET", path, &[], b"").into_parts();
        env.dav.path_case().rewrite_request(&mut parts);
        assert_eq!(parts.uri.path(), path);
    }
}

// --- TestCaseInsensitiveHost: NTFS/APFS, where the filesystem itself opens `Kingdom` as
// `kingdom` ---------------------------------------------------------------------------

/// WebDAV deletes an existing MOVE destination first (`Overwrite: T`); on a
/// case-insensitive host that destination IS the source. A hard link reproduces "two
/// spellings, one file" on a case-sensitive test host.
#[tokio::test]
async fn a_moves_case_variant_destination_is_not_an_existing_resource() {
    let env = Env::new();
    let series = env.lib("series");
    std::fs::hard_link(series.join("vol1.cbz"), series.join("VOL1.cbz")).unwrap();
    std::fs::write(series.join("vol1.mokuro"), b"{}").unwrap();

    // Not a MOVE: the other spelling is a resource like any other.
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/series/VOL1.cbz", &[], b"")
            .await
            .code(),
        200
    );

    let d = dest("/mokuro-reader/series/VOL1.cbz");
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series/vol1.cbz",
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
            .has("rename_volume_upload series/vol1.cbz series/VOL1.cbz")
    );
    assert!(series.join("vol1.mokuro").exists());
    assert_eq!(std::fs::read(series.join("VOL1.cbz")).unwrap(), b"volume 1");
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
        "/mokuro-reader/kingdom/v80.cbz",
    ] {
        let r = req(&env, Some("uploader"), "PUT", path, &[], &cbz_bytes(1)).await;
        assert!(matches!(r.code(), 200 | 201 | 204), "{path}: {}", r.text());
    }
    assert_eq!(names(&env.lib("Kingdom")), ["v01.cbz", "v80.cbz"]);
    assert!(!names(&env.lib("")).contains(&"kingdom".to_string()));
}

/// The prod scenario after the 0.5.3 cutover: `Kingdom/` holds every volume, and a reader
/// still asks for `kingdom/...`.
#[tokio::test]
async fn the_kingdom_scenario() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Kingdom")).unwrap();
    for v in 1..=80 {
        std::fs::write(env.lib(&format!("Kingdom/第{v:02}巻.cbz")), b"cbz").unwrap();
    }
    std::fs::write(
        env.lib("Kingdom/第80巻.mokuro"),
        b"{\"title\": \"Kingdom\"}",
    )
    .unwrap();
    let r = req(
        &env,
        None,
        "GET",
        "/mokuro-reader/kingdom/%E7%AC%AC80%E5%B7%BB.mokuro",
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
    // The folder and its 81 files, every href in the on-disk spelling.
    assert_eq!(
        text.matches("/mokuro-reader/Kingdom/").count(),
        82,
        "{text}"
    );
    assert!(!text.contains("/mokuro-reader/kingdom"));
    assert!(text.contains("/mokuro-reader/Kingdom/%E7%AC%AC80%E5%B7%BB.mokuro"));
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
