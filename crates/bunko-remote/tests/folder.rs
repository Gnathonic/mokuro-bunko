//! The local folder driver on a real temporary folder (plan §4.1). Every test reads
//! the probe and asserts what this host's filesystem does: on Linux (tmpfs, ext4) the
//! names are exact, on the CI's Windows (NTFS) and macOS (APFS) runners they fold.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bunko_remote::backend::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, RNode, UploadSource,
    UploadTarget, read_to_vec,
};
use bunko_remote::folder::{FolderBackend, FolderConfig, account_id, snapshot_path};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const ROOT_NAME: &str = "mokuro-reader";

struct Lib {
    tmp: TempDir,
    /// The library folder (named `Manga`, reported as `mokuro-reader`).
    root: PathBuf,
}

impl Lib {
    fn new() -> Lib {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("Manga");
        std::fs::create_dir(&root).unwrap();
        Lib { tmp, root }
    }

    fn state(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    fn cfg(&self, watch: bool) -> FolderConfig {
        FolderConfig {
            path: self.root.clone(),
            root_name: ROOT_NAME.into(),
            state_dir: self.state(),
            watch,
        }
    }

    async fn open(&self) -> FolderBackend {
        FolderBackend::connect(self.cfg(false)).await.unwrap()
    }

    async fn open_watching(&self) -> FolderBackend {
        FolderBackend::connect(self.cfg(true)).await.unwrap()
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    /// An outside writer (the reader, Explorer) puts `rel`.
    fn put(&self, rel: &str, bytes: &[u8]) {
        let p = self.abs(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn mkdir(&self, rel: &str) {
        std::fs::create_dir_all(self.abs(rel)).unwrap();
    }

    fn stage(&self, bytes: &[u8]) -> (PathBuf, [u8; 32]) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = self.tmp.path().join(format!("staged-{n}"));
        std::fs::write(&p, bytes).unwrap();
        (p, Sha256::digest(bytes).into())
    }

    /// Every `.bunko-*.tmp` anywhere in the folder.
    fn temps(&self) -> Vec<PathBuf> {
        walk(&self.root)
            .into_iter()
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
                n.starts_with(".bunko-") && n.ends_with(".tmp")
            })
            .collect()
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() {
                stack.push(e.path());
            }
            out.push(e.path());
        }
    }
    out
}

fn op() -> OpCtx {
    OpCtx {
        op_id: "17".into(),
        resume: None,
    }
}

async fn upload_new(
    lib: &Lib,
    b: &FolderBackend,
    parent: &str,
    name: &str,
    bytes: &[u8],
) -> Result<RNode, BErr> {
    let (path, sha256) = lib.stage(bytes);
    b.upload(
        UploadTarget::New { parent, name },
        UploadSource::File {
            path: &path,
            size: bytes.len() as u64,
            sha256,
        },
        &mut op(),
    )
    .await
}

async fn replace(lib: &Lib, b: &FolderBackend, pid: &str, bytes: &[u8]) -> Result<RNode, BErr> {
    let (path, sha256) = lib.stage(bytes);
    b.upload(
        UploadTarget::Replace { pid, if_cver: None },
        UploadSource::File {
            path: &path,
            size: bytes.len() as u64,
            sha256,
        },
        &mut op(),
    )
    .await
}

async fn list_all(b: &FolderBackend) -> Vec<RNode> {
    let mut v = Vec::new();
    b.list_all(&[String::new()], &mut |n| v.push(n))
        .await
        .unwrap();
    v
}

async fn pids(b: &FolderBackend) -> Vec<String> {
    let mut v: Vec<String> = list_all(b).await.into_iter().map(|n| n.pid).collect();
    v.sort();
    v
}

async fn list_dir(b: &FolderBackend, dir: &str) -> Result<Vec<RNode>, BErr> {
    let mut v = Vec::new();
    b.list_dir(dir, &mut |n| v.push(n)).await?;
    Ok(v)
}

async fn read_all(
    b: &FolderBackend,
    pid: &str,
    range: Option<(u64, Option<u64>)>,
) -> Result<Vec<u8>, BErr> {
    let body = b.read(pid, None, range).await?;
    Ok(read_to_vec(body).await.unwrap())
}

/// The change page at `c`.
async fn page(b: &FolderBackend, c: &Cursor) -> (Vec<ChangeItem>, Cursor) {
    match b.changes(c).await.unwrap() {
        ChangePage::Page { items, next, more } => {
            assert!(!more);
            (items, next)
        }
        ChangePage::CursorLost => panic!("cursor lost"),
    }
}

fn upserts(items: &[ChangeItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Upsert(n) => Some(n.pid.clone()),
            ChangeItem::Removed { .. } => None,
        })
        .collect()
}

fn removals(items: &[ChangeItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.clone()),
            ChangeItem::Upsert(_) => None,
        })
        .collect()
}

fn echo(n: &RNode) -> (String, Option<String>, Vec<u8>, String, u64) {
    (
        n.pid.clone(),
        n.parent.clone(),
        n.name_raw.clone(),
        n.cver.clone(),
        n.size,
    )
}

// ---- 1. the probe ------------------------------------------------------------------

#[tokio::test]
async fn the_probe_matches_the_filesystem() {
    let lib = Lib::new();
    let b = lib.open().await;
    let p = b.probe();
    assert!(p.reachable && p.writable, "{p:?}");
    std::fs::write(lib.root.join("Aa"), b"x").unwrap();
    let folds = std::fs::symlink_metadata(lib.root.join("aA")).is_ok();
    assert_eq!(p.folds_case, folds, "{p:?}");
    // A run on a folding filesystem on purpose (a casefold tmpfs, say) says so.
    if let Ok(want) = std::env::var("BUNKO_TEST_EXPECT_FOLDS_CASE") {
        assert_eq!(p.folds_case, want == "1", "{p:?}");
    }
    assert!(p.mtime_granularity_ns > 0, "{p:?}");
    assert!(!p.watcher, "no watcher was asked for");
    // The probe leaves nothing behind.
    let left: Vec<_> = walk(&lib.root)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains(".bunko-probe-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // On the hosts CI runs on, a create that refuses to replace works.
    if cfg!(any(target_os = "linux", windows, target_os = "macos")) {
        assert!(b.caps().cas_create, "{p:?}");
    }
    assert!(b.caps().path_ids && b.caps().push && !b.caps().cas_overwrite);
}

// ---- 2. roots ----------------------------------------------------------------------

#[tokio::test]
async fn roots_are_the_folder_under_the_configured_name() {
    let lib = Lib::new();
    let b = lib.open().await;
    let roots = b.locate_roots(ROOT_NAME).await.unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].pid, "");
    assert_eq!(roots[0].parent, None);
    assert_eq!(
        roots[0].name_raw,
        ROOT_NAME.as_bytes(),
        "the folder is named Manga"
    );
    assert!(roots[0].is_dir);
    assert!(b.locate_roots("Manga").await.unwrap().is_empty());
    assert!(b.locate_roots("other").await.unwrap().is_empty());
    assert_eq!(
        b.stat("").await.unwrap().unwrap().name_raw,
        ROOT_NAME.as_bytes()
    );
    let acct = b.account().await.unwrap();
    assert_eq!(acct.id, account_id(&lib.root));
    assert_eq!(acct.label, acct.id);
}

#[tokio::test]
async fn a_missing_folder_is_created_one_level_only() {
    let tmp = TempDir::new().unwrap();
    let cfg = |path: PathBuf| FolderConfig {
        path,
        root_name: ROOT_NAME.into(),
        state_dir: tmp.path().join("state"),
        watch: false,
    };
    // The parent is there: no root yet, and Locate may offer to create it.
    let b = FolderBackend::connect(cfg(tmp.path().join("Manga")))
        .await
        .unwrap();
    assert!(!b.probe().reachable);
    assert!(b.locate_roots(ROOT_NAME).await.unwrap().is_empty());
    assert_eq!(b.stat("").await.unwrap(), None);
    let root = b.create_root(ROOT_NAME).await.unwrap();
    assert_eq!(root.pid, "");
    assert_eq!(root.name_raw, ROOT_NAME.as_bytes());
    assert!(tmp.path().join("Manga").is_dir());
    assert!(b.probe().reachable && b.probe().writable);
    assert_eq!(b.create_root(ROOT_NAME).await, Err(BErr::NameConflict));
    // The parent is missing too: the drive is away. Nothing is created.
    let away = tmp.path().join("gone/Manga");
    let b = FolderBackend::connect(cfg(away.clone())).await.unwrap();
    assert!(matches!(
        b.locate_roots(ROOT_NAME).await,
        Err(BErr::Transient(_))
    ));
    assert!(matches!(b.stat("").await, Err(BErr::Transient(_))));
    assert!(matches!(b.stat("S/x.cbz").await, Err(BErr::Transient(_))));
    assert!(matches!(b.account().await, Err(BErr::Transient(_))));
    assert_eq!(b.quota().await, Ok(None));
    assert_eq!(b.create_root(ROOT_NAME).await, Err(BErr::NotFound));
    assert!(!away.exists());
}

// ---- 3. listing filters -------------------------------------------------------------

#[tokio::test]
async fn listings_hide_internal_names_swap_files_and_links() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"zip");
    lib.put(".mokuro-bunko.json", b"{}");
    lib.put(".mokuro-queue.json", b"{}");
    // Bunko's own.
    lib.put(".bunko-0123456789abcdef.tmp", b"staged");
    lib.put("S/.bunko-case-77.tmp", b"half a rename");
    lib.put("S/.Vol 2.cbz.bunko-9.tmp", b"webdav-style");
    lib.mkdir(".bunko-probe-x");
    lib.put(
        ".mokuro-bunko-trash/20260101T000000Z-deadbeef/S/a.cbz.bunko-trashed",
        b"t",
    );
    lib.put("S/old.cbz.bunko-trashed", b"t");
    // The system's, at the top only.
    lib.mkdir("$RECYCLE.BIN");
    lib.mkdir("lost+found");
    lib.mkdir("S/lost+found");
    // A reader write in progress: an empty file beside its swap file.
    lib.put("S/new.cbz", b"");
    lib.put("S/new.cbz.crswap", b"half");
    // An overwrite in progress: the old bytes are still the file.
    lib.put("S/kept.cbz", b"old bytes");
    lib.put("S/kept.cbz.crswap", b"new");
    // The same while an earlier swap file was left behind: Chromium numbers the next.
    lib.put("S/again.cbz", b"");
    lib.put("S/again.cbz.crswap", b"stale");
    lib.put("S/again.cbz.1.crswap", b"half");
    lib.put("S/other.cbz", b"");
    lib.put("S/other.cbz.2.crswap", b"half");
    // An empty file without a swap file is a file.
    lib.put("S/empty.txt", b"");
    let b = lib.open().await;
    let mut want = vec![
        ".mokuro-bunko.json",
        ".mokuro-queue.json",
        "S",
        "S/Vol 1.cbz",
        "S/empty.txt",
        "S/kept.cbz",
        "S/lost+found",
    ];
    want.sort();
    assert_eq!(pids(&b).await, want);
    for hidden in [
        "S/new.cbz",
        "S/new.cbz.crswap",
        "S/other.cbz",
        "S/again.cbz",
        "S/again.cbz.1.crswap",
        "lost+found",
        ".mokuro-bunko-trash",
    ] {
        assert_eq!(b.stat(hidden).await.unwrap(), None, "{hidden}");
    }
    assert!(b.stat("S/kept.cbz").await.unwrap().is_some());
    let named = b.find_named("S", "new.cbz").await.unwrap().0;
    assert!(named.is_empty(), "{named:?}");
    // The swap lands: now it is listed.
    std::fs::rename(lib.abs("S/new.cbz.crswap"), lib.abs("S/new.cbz")).unwrap();
    assert!(pids(&b).await.contains(&"S/new.cbz".to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn links_are_not_followed_and_unsafe_names_are_reported() {
    use std::os::unix::ffi::OsStrExt;
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"zip");
    let outside = lib.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.cbz"), b"x").unwrap();
    std::os::unix::fs::symlink(&outside, lib.abs("S/link")).unwrap();
    std::os::unix::fs::symlink(lib.abs("S/Vol 1.cbz"), lib.abs("S/alias.cbz")).unwrap();
    // A name the index refuses is still the driver's to report.
    lib.put("S/a:b.cbz", b"colon");
    if cfg!(target_os = "linux") {
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.cbz");
        std::fs::write(lib.abs("S").join(bad), b"latin-1").unwrap();
    }
    let b = lib.open().await;
    assert_eq!(pids(&b).await, ["S", "S/Vol 1.cbz", "S/a:b.cbz"]);
    assert_eq!(b.stat("S/link").await.unwrap(), None);
    assert_eq!(b.stat("S/alias.cbz").await.unwrap(), None);
    assert!(matches!(
        read_all(&b, "S/alias.cbz", None).await,
        Err(BErr::NotFound)
    ));
}

/// A folder link inside the library that leads out of it (a symbolic link on Unix, a
/// junction on Windows) and the files below it: the outside folder is never touched.
/// Listings skip the link; `stat` and `find_named` see nothing below it; no read,
/// write, mkdir, move, copy, trash or delete reaches through it, in either direction.
async fn nothing_goes_through_a_folder_link(lib: &Lib, outside: &Path) {
    let snapshot = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
        let mut v: Vec<(PathBuf, Vec<u8>)> = walk(dir)
            .into_iter()
            .map(|p| {
                let bytes = if p.is_file() {
                    std::fs::read(&p).unwrap()
                } else {
                    Vec::new()
                };
                (p, bytes)
            })
            .collect();
        v.sort();
        v
    };
    let before = snapshot(outside);
    let b = lib.open().await;
    assert_eq!(pids(&b).await, ["S", "S/Vol 1.cbz"]);
    assert_eq!(b.stat("Link").await.unwrap(), None);
    assert_eq!(b.stat("Link/Vol 9.cbz").await.unwrap(), None);
    assert_eq!(b.stat("Link/sub/Deep.cbz").await.unwrap(), None);
    assert!(matches!(list_dir(&b, "Link").await, Err(BErr::NotFound)));
    assert!(matches!(
        b.find_named("Link", "Vol 9.cbz").await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        read_all(&b, "Link/Vol 9.cbz", None).await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        upload_new(lib, &b, "Link", "planted.cbz", b"planted").await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        upload_new(lib, &b, "Link/sub", "planted.cbz", b"planted").await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        replace(lib, &b, "Link/Vol 9.cbz", b"overwritten").await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        b.mkdir("Link", "made", &op()).await,
        Err(BErr::NotFound)
    ));
    // Out of the link, and into it.
    assert!(
        b.update("Link/Vol 9.cbz", Some("S"), Some("Taken.cbz"), None)
            .await
            .is_err()
    );
    assert!(
        b.update("Link/sub", Some(""), Some("Taken"), None)
            .await
            .is_err()
    );
    assert!(
        b.update("S/Vol 1.cbz", Some("Link"), None, None)
            .await
            .is_err()
    );
    assert!(b.copy("Link/Vol 9.cbz", "S", "Copied.cbz").await.is_err());
    assert!(b.copy("S/Vol 1.cbz", "Link", "Copied.cbz").await.is_err());
    assert!(matches!(
        b.trash("Link/Vol 9.cbz", None).await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        b.trash("Link/sub", None).await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        b.delete_permanent("Link/Vol 9.cbz").await,
        Err(BErr::NotFound)
    ));
    assert!(matches!(
        b.delete_permanent("Link/sub/Deep.cbz").await,
        Err(BErr::NotFound)
    ));
    assert_eq!(snapshot(outside), before, "the outside folder changed");
    assert_eq!(std::fs::read(lib.abs("S/Vol 1.cbz")).unwrap(), b"mine");
    assert!(!lib.abs("S/Taken.cbz").exists() && !lib.abs("Taken").exists());
    assert!(!lib.abs("S/Copied.cbz").exists());
    // The link itself is the library's to leave alone too.
    assert!(matches!(b.trash("Link", None).await, Err(BErr::NotFound)));
    assert_eq!(pids(&b).await, ["S", "S/Vol 1.cbz"]);
    assert_eq!(snapshot(outside), before);
}

/// A library folder with `Link`, a folder link to `outside`, which holds
/// `Vol 9.cbz` and `sub/Deep.cbz`.
fn outside_folder(lib: &Lib) -> PathBuf {
    lib.put("S/Vol 1.cbz", b"mine");
    let outside = lib.tmp.path().join("outside");
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::write(outside.join("Vol 9.cbz"), b"not the library's").unwrap();
    std::fs::write(outside.join("sub/Deep.cbz"), b"deep").unwrap();
    outside
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_folder_never_leads_outside_the_library() {
    let lib = Lib::new();
    let outside = outside_folder(&lib);
    std::os::unix::fs::symlink(&outside, lib.abs("Link")).unwrap();
    nothing_goes_through_a_folder_link(&lib, &outside).await;
}

#[cfg(windows)]
#[tokio::test]
async fn a_junction_never_leads_outside_the_library() {
    let lib = Lib::new();
    let outside = outside_folder(&lib);
    let made = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(lib.abs("Link"))
        .arg(&outside)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    nothing_goes_through_a_folder_link(&lib, &outside).await;
}

/// The trash folder a link to elsewhere: a trash is refused, the file stays, and the
/// daily purge never empties what the link leads to.
#[cfg(unix)]
#[tokio::test]
async fn a_linked_trash_folder_is_never_followed() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"mine");
    let outside = lib.tmp.path().join("outside");
    let old_batch = outside.join("20000101T000000Z-deadbeef");
    std::fs::create_dir_all(&old_batch).unwrap();
    std::fs::write(old_batch.join("keep.txt"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, lib.abs(".mokuro-bunko-trash")).unwrap();
    let b = lib.open().await;
    assert!(b.trash("S/Vol 1.cbz", None).await.is_err());
    assert_eq!(std::fs::read(lib.abs("S/Vol 1.cbz")).unwrap(), b"mine");
    let c = b.start_cursor().await.unwrap();
    let _ = b.changes(&c).await;
    assert_eq!(std::fs::read(old_batch.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "nothing was trashed into the link's target"
    );
}

// ---- 4. uploads --------------------------------------------------------------------

#[tokio::test]
async fn uploads_echo_what_the_next_listing_shows() {
    let lib = Lib::new();
    lib.mkdir("S");
    let b = lib.open().await;
    let made = upload_new(&lib, &b, "S", "Vol 1.cbz", b"zipbytes")
        .await
        .unwrap();
    assert_eq!(made.pid, "S/Vol 1.cbz");
    assert_eq!(made.size, 8);
    let listed = list_all(&b)
        .await
        .into_iter()
        .find(|n| n.pid == made.pid)
        .unwrap();
    assert_eq!(echo(&made), echo(&listed));
    assert_eq!(std::fs::read(lib.abs("S/Vol 1.cbz")).unwrap(), b"zipbytes");
    // A replace: same pid, new version, the listing agrees.
    let rep = replace(&lib, &b, &made.pid, b"other bytes").await.unwrap();
    assert_eq!(rep.pid, made.pid);
    assert_ne!(rep.cver, made.cver);
    let listed = list_all(&b)
        .await
        .into_iter()
        .find(|n| n.pid == made.pid)
        .unwrap();
    assert_eq!(echo(&rep), echo(&listed));
    // At the top too.
    let top = upload_new(&lib, &b, "", "catalog.json", b"{}")
        .await
        .unwrap();
    assert_eq!(
        (top.pid.as_str(), top.parent.as_deref()),
        ("catalog.json", Some(""))
    );
    assert!(lib.temps().is_empty(), "{:?}", lib.temps());
}

#[tokio::test]
async fn a_create_never_replaces() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"theirs");
    lib.mkdir("S/Folder");
    let b = lib.open().await;
    assert_eq!(
        upload_new(&lib, &b, "S", "Vol 1.cbz", b"mine").await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(
        upload_new(&lib, &b, "S", "Folder", b"mine").await,
        Err(BErr::NameConflict)
    );
    let variant = upload_new(&lib, &b, "S", "VOL 1.cbz", b"mine").await;
    if b.probe().folds_case {
        assert_eq!(variant, Err(BErr::NameConflict));
    } else {
        assert_eq!(variant.unwrap().pid, "S/VOL 1.cbz");
    }
    assert_eq!(std::fs::read(lib.abs("S/Vol 1.cbz")).unwrap(), b"theirs");
    assert_eq!(
        upload_new(&lib, &b, "Nope", "x.cbz", b"mine").await,
        Err(BErr::NotFound)
    );
    assert_eq!(
        replace(&lib, &b, "S/missing.cbz", b"x").await,
        Err(BErr::NotFound)
    );
    assert_eq!(
        replace(&lib, &b, "S/Folder", b"x").await,
        Err(BErr::NotFound)
    );
    for bad in ["", "..", ".bunko-1.tmp", "x.crswap", "a.bunko-trashed"] {
        assert_eq!(
            upload_new(&lib, &b, "S", bad, b"x").await,
            Err(BErr::InvalidName),
            "{bad:?}"
        );
    }
    assert!(lib.temps().is_empty(), "{:?}", lib.temps());
}

#[tokio::test]
async fn staged_bytes_that_changed_are_refused_and_leave_nothing() {
    let lib = Lib::new();
    lib.mkdir("S");
    lib.put("S/kept.cbz", b"old");
    let b = lib.open().await;
    let (path, _) = lib.stage(b"what was hashed");
    let wrong: [u8; 32] = Sha256::digest(b"something else").into();
    for to in [
        UploadTarget::New {
            parent: "S",
            name: "new.cbz",
        },
        UploadTarget::Replace {
            pid: "S/kept.cbz",
            if_cver: None,
        },
    ] {
        let r = b
            .upload(
                to,
                UploadSource::File {
                    path: &path,
                    size: 15,
                    sha256: wrong,
                },
                &mut op(),
            )
            .await;
        assert!(matches!(r, Err(BErr::Transient(_))), "{r:?}");
    }
    assert!(!lib.abs("S/new.cbz").exists());
    assert_eq!(std::fs::read(lib.abs("S/kept.cbz")).unwrap(), b"old");
    assert!(lib.temps().is_empty(), "{:?}", lib.temps());
}

// ---- 5. the other mutations ----------------------------------------------------------

#[tokio::test]
async fn mkdir_update_copy_trash_and_delete_follow_the_fake() {
    let lib = Lib::new();
    lib.put("A/x.cbz", b"x");
    lib.put("A/sub/y.cbz", b"y");
    lib.put("B/z.cbz", b"z");
    let b = lib.open().await;
    // mkdir
    let c = b.mkdir("", "C", &op()).await.unwrap();
    assert_eq!((c.pid.as_str(), c.is_dir), ("C", true));
    assert_eq!(b.mkdir("", "C", &op()).await, Err(BErr::NameConflict));
    assert_eq!(b.mkdir("", "A/x.cbz", &op()).await, Err(BErr::InvalidName));
    assert_eq!(b.mkdir("Nope", "D", &op()).await, Err(BErr::NotFound));
    // update
    assert!(matches!(
        b.update("A", Some("A/sub"), None, None).await,
        Err(BErr::Fatal(_))
    ));
    assert!(matches!(
        b.update("A", Some("A"), None, None).await,
        Err(BErr::Fatal(_))
    ));
    assert_eq!(
        b.update("", Some("C"), None, None).await,
        Err(BErr::Unsupported)
    );
    assert_eq!(
        b.update("A/x.cbz", Some("B"), Some("z.cbz"), None).await,
        Err(BErr::NameConflict)
    );
    assert_eq!(
        b.update("A/none.cbz", Some("B"), None, None).await,
        Err(BErr::NotFound)
    );
    assert_eq!(
        b.update("A/x.cbz", Some("Nope"), None, None).await,
        Err(BErr::NotFound)
    );
    assert_eq!(
        b.update("A/x.cbz", None, Some("w.cbz"), Some("1-1")).await,
        Err(BErr::PreconditionFailed)
    );
    let same = b.update("A/x.cbz", None, None, None).await.unwrap();
    assert_eq!(same.pid, "A/x.cbz");
    let moved = b.update("A", Some("C"), Some("A2"), None).await.unwrap();
    assert_eq!(
        (moved.pid.as_str(), moved.parent.as_deref()),
        ("C/A2", Some("C"))
    );
    assert_eq!(read_all(&b, "C/A2/sub/y.cbz", None).await.unwrap(), b"y");
    assert_eq!(b.stat("A").await.unwrap(), None);
    let file = b
        .update("B/z.cbz", Some(""), Some("zz.cbz"), None)
        .await
        .unwrap();
    assert_eq!(file.pid, "zz.cbz");
    // copy
    assert_eq!(b.copy("C/A2", "", "copy").await, Err(BErr::Unsupported));
    assert_eq!(b.copy("C/none.cbz", "", "copy").await, Err(BErr::NotFound));
    assert_eq!(
        b.copy("zz.cbz", "C/A2", "x.cbz").await,
        Err(BErr::NameConflict)
    );
    let copied = b.copy("zz.cbz", "B", "copy.cbz").await.unwrap();
    assert_eq!(copied.pid, "B/copy.cbz");
    assert_eq!(read_all(&b, "B/copy.cbz", None).await.unwrap(), b"z");
    // trash
    assert_eq!(b.trash("", None).await, Err(BErr::Unsupported));
    assert_eq!(b.trash("B/none.cbz", None).await, Err(BErr::NotFound));
    assert_eq!(
        b.trash("B/copy.cbz", Some("1-1")).await,
        Err(BErr::PreconditionFailed)
    );
    b.trash("B/copy.cbz", Some(&copied.cver)).await.unwrap();
    assert_eq!(b.stat("B/copy.cbz").await.unwrap(), None);
    b.trash("C/A2", None).await.unwrap();
    assert_eq!(b.stat("C/A2").await.unwrap(), None);
    // delete_permanent
    assert_eq!(b.delete_permanent("").await, Err(BErr::Unsupported));
    assert_eq!(b.delete_permanent("none.cbz").await, Err(BErr::NotFound));
    assert_eq!(b.delete_permanent("C").await, Err(BErr::Unsupported));
    b.delete_permanent("zz.cbz").await.unwrap();
    assert!(!lib.abs("zz.cbz").exists());
    assert_eq!(pids(&b).await, ["B", "C"]);
    assert!(lib.temps().is_empty(), "{:?}", lib.temps());
}

// ---- 6. case-only renames ------------------------------------------------------------

#[tokio::test]
async fn case_only_renames_work_on_every_host() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"one");
    lib.put("Series/Vol 2.cbz", b"two");
    lib.put("Series/Vol 2.mokuro", b"{}");
    let b = lib.open().await;
    let f = b
        .update("S/Vol 1.cbz", None, Some("vol 1.cbz"), None)
        .await
        .unwrap();
    assert_eq!(f.pid, "S/vol 1.cbz");
    assert_eq!(f.name_raw, b"vol 1.cbz");
    let d = b
        .update("Series", None, Some("series"), None)
        .await
        .unwrap();
    assert_eq!(d.pid, "series");
    assert_eq!(
        pids(&b).await,
        [
            "S",
            "S/vol 1.cbz",
            "series",
            "series/Vol 2.cbz",
            "series/Vol 2.mokuro"
        ]
    );
    assert_eq!(b.stat("S/Vol 1.cbz").await.unwrap(), None);
    assert_eq!(b.stat("Series").await.unwrap(), None);
    assert_eq!(b.stat("Series/Vol 2.cbz").await.unwrap(), None);
    assert!(b.stat("series/Vol 2.cbz").await.unwrap().is_some());
    assert_eq!(read_all(&b, "S/vol 1.cbz", None).await.unwrap(), b"one");
    // The move already happened (a retry after a crash): the destination answers.
    let again = b
        .update("S/Vol 1.cbz", None, Some("vol 1.cbz"), None)
        .await
        .unwrap();
    assert_eq!(again.pid, "S/vol 1.cbz");
    // Both spellings at once (a case-sensitive host): two names, no merge here.
    if !b.probe().folds_case {
        lib.put("T/a.cbz", b"a");
        lib.put("T/A.cbz", b"A");
        assert_eq!(
            b.update("T/a.cbz", None, Some("A.cbz"), None).await,
            Err(BErr::NameConflict)
        );
        assert_eq!(std::fs::read(lib.abs("T/A.cbz")).unwrap(), b"A");
    }
    assert!(!lib.state().join("folder-journal.json").exists());
    assert!(
        walk(&lib.root)
            .iter()
            .all(|p| !p.to_string_lossy().contains(".bunko-case-"))
    );
}

#[tokio::test]
async fn an_interrupted_case_rename_is_finished_at_connect() {
    let lib = Lib::new();
    lib.put("Kingdom/Vol 1.cbz", b"one");
    let b = lib.open().await;
    b.stop_next_case_rename();
    assert!(matches!(
        b.update("Kingdom", None, Some("kingdom"), None).await,
        Err(BErr::Transient(_))
    ));
    drop(b);
    // The crash left the folder under its temporary name, which nobody lists.
    assert!(lib.state().join("folder-journal.json").exists());
    let b = lib.open().await;
    assert_eq!(pids(&b).await, ["kingdom", "kingdom/Vol 1.cbz"]);
    assert_eq!(
        read_all(&b, "kingdom/Vol 1.cbz", None).await.unwrap(),
        b"one"
    );
    assert!(!lib.state().join("folder-journal.json").exists());
    assert!(
        walk(&lib.root)
            .iter()
            .all(|p| !p.to_string_lossy().contains(".bunko-case-"))
    );
}

#[tokio::test]
async fn an_interrupted_case_rename_is_finished_when_the_folder_comes_back() {
    let lib = Lib::new();
    let share = lib.tmp.path().join("mnt/share");
    let root = share.join("Manga");
    std::fs::create_dir_all(root.join("Kingdom")).unwrap();
    std::fs::write(root.join("Kingdom/Vol 1.cbz"), b"one").unwrap();
    let cfg = FolderConfig {
        path: root.clone(),
        root_name: ROOT_NAME.into(),
        state_dir: lib.state(),
        watch: false,
    };
    let b = FolderBackend::connect(cfg.clone()).await.unwrap();
    b.stop_next_case_rename();
    assert!(
        b.update("Kingdom", None, Some("kingdom"), None)
            .await
            .is_err()
    );
    drop(b);
    // Restarted while the share is away, then it comes back.
    let away = lib.tmp.path().join("mnt/share-away");
    std::fs::rename(&share, &away).unwrap();
    let b = FolderBackend::connect(cfg).await.unwrap();
    assert!(matches!(
        b.locate_roots(ROOT_NAME).await,
        Err(BErr::Transient(_))
    ));
    std::fs::rename(&away, &share).unwrap();
    assert_eq!(pids(&b).await, ["kingdom", "kingdom/Vol 1.cbz"]);
    assert!(!lib.state().join("folder-journal.json").exists());
}

// ---- 7. the change feed ----------------------------------------------------------------

#[tokio::test]
async fn changes_report_what_happened_outside() {
    let lib = Lib::new();
    lib.put("A/x.cbz", b"x");
    lib.put("A/sub/y.cbz", b"y");
    lib.put("B/keep.cbz", b"k");
    lib.put("B/edit.cbz", b"1");
    lib.put("B/gone.cbz", b"g");
    let b = lib.open().await;
    let c0 = b.start_cursor().await.unwrap();
    assert!(snapshot_path(&lib.state()).exists());
    // Nothing happened: the same cursor back, nothing saved.
    let (items, c) = page(&b, &c0).await;
    assert!(items.is_empty(), "{items:?}");
    assert_eq!(c, c0);
    // Outside: a create, an edit, a delete, a folder move.
    lib.put("B/new/n.cbz", b"n");
    std::fs::write(lib.abs("B/edit.cbz"), b"22").unwrap();
    std::fs::remove_file(lib.abs("B/gone.cbz")).unwrap();
    std::fs::rename(lib.abs("A"), lib.abs("Z")).unwrap();
    let (items, c1) = page(&b, &c0).await;
    assert_ne!(c1, c0);
    let ups = upserts(&items);
    let mut sorted = ups.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "B/edit.cbz",
            "B/new",
            "B/new/n.cbz",
            "Z",
            "Z/sub",
            "Z/sub/y.cbz",
            "Z/x.cbz"
        ]
    );
    for (i, p) in ups.iter().enumerate() {
        let parent = p.rsplit_once('/').map_or("", |x| x.0);
        if let Some(j) = ups.iter().position(|q| q == parent) {
            assert!(j < i, "{parent} after {p}: {ups:?}");
        }
    }
    let rem = removals(&items);
    let mut sorted = rem.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        ["A", "A/sub", "A/sub/y.cbz", "A/x.cbz", "B/gone.cbz"]
    );
    for (i, p) in rem.iter().enumerate() {
        for q in &rem[..i] {
            assert!(!p.starts_with(&format!("{q}/")), "{q} before its child {p}");
        }
    }
    // Upserts come before removals.
    let first_removal = items
        .iter()
        .position(|i| matches!(i, ChangeItem::Removed { .. }))
        .unwrap();
    assert!(
        items[first_removal..]
            .iter()
            .all(|i| matches!(i, ChangeItem::Removed { .. }))
    );
    // At least once: the same cursor again gives the same items.
    let (again, c1b) = page(&b, &c0).await;
    assert_eq!(again, items);
    assert_eq!(c1b, c1);
    // Moving on: nothing new.
    let (items, c2) = page(&b, &c1).await;
    assert!(items.is_empty(), "{items:?}");
    assert_eq!(c2, c1);
    // The old base is gone now, and a cursor of another feed never matches.
    assert_eq!(b.changes(&c0).await.unwrap(), ChangePage::CursorLost);
    let (epoch, _) = c1.0.rsplit_once(':').unwrap();
    assert_eq!(
        b.changes(&Cursor(format!("{epoch}:99"))).await.unwrap(),
        ChangePage::CursorLost
    );
    assert_eq!(
        b.changes(&Cursor("other-epoch:2".into())).await.unwrap(),
        ChangePage::CursorLost
    );
    // A reopened backend keeps the feed.
    drop(b);
    let b = lib.open().await;
    lib.put("late.cbz", b"l");
    let (items, _) = page(&b, &c1).await;
    assert_eq!(upserts(&items), ["late.cbz"]);
}

#[tokio::test]
async fn an_unreachable_folder_reports_no_removals() {
    let lib = Lib::new();
    // The library sits two levels down, like a mounted share.
    let mount = lib.tmp.path().join("mnt");
    let root = mount.join("share/Manga");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("x.cbz"), b"x").unwrap();
    let cfg = FolderConfig {
        path: root.clone(),
        root_name: ROOT_NAME.into(),
        state_dir: lib.state(),
        watch: false,
    };
    let b = FolderBackend::connect(cfg).await.unwrap();
    let c0 = b.start_cursor().await.unwrap();
    // Unplugged: the folder's parent is gone.
    let away = lib.tmp.path().join("mnt-away");
    std::fs::rename(mount.join("share"), &away).unwrap();
    assert!(matches!(b.changes(&c0).await, Err(BErr::Transient(_))));
    let mut n = 0;
    assert!(matches!(
        b.list_all(&[String::new()], &mut |_| n += 1).await,
        Err(BErr::Transient(_))
    ));
    assert!(matches!(b.stat("x.cbz").await, Err(BErr::Transient(_))));
    // Back: nothing changed.
    std::fs::rename(&away, mount.join("share")).unwrap();
    let (items, c) = page(&b, &c0).await;
    assert!(items.is_empty(), "{items:?}");
    assert_eq!(c, c0);
}

#[tokio::test]
async fn a_deleted_folder_reports_its_removals() {
    let lib = Lib::new();
    lib.put("S/x.cbz", b"x");
    let b = lib.open().await;
    let c0 = b.start_cursor().await.unwrap();
    std::fs::remove_dir_all(&lib.root).unwrap();
    let (items, _) = page(&b, &c0).await;
    assert_eq!(removals(&items), ["S/x.cbz", "S", ""]);
    assert!(upserts(&items).is_empty());
    let mut n = 0;
    assert_eq!(
        b.list_all(&[String::new()], &mut |_| n += 1).await,
        Err(BErr::NotFound)
    );
    assert!(b.locate_roots(ROOT_NAME).await.unwrap().is_empty());
}

/// A folder Bunko may not read keeps what it held: no removals for it in the feed (a
/// later diff would otherwise fail on it forever), and a full listing skips it.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_folder_keeps_its_entries() {
    use std::os::unix::fs::PermissionsExt;
    let lib = Lib::new();
    lib.put("S/a.cbz", b"a");
    lib.put("T/b.cbz", b"b");
    lib.put("T/sub/c.cbz", b"c");
    let b = lib.open().await;
    let c0 = b.start_cursor().await.unwrap();
    std::fs::set_permissions(lib.abs("T"), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(lib.abs("T")).is_ok() {
        // Running as root: permissions do not apply.
        std::fs::set_permissions(lib.abs("T"), std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    lib.put("S/new.cbz", b"n");
    let (items, c1) = page(&b, &c0).await;
    assert_eq!(upserts(&items), ["S/new.cbz"]);
    assert!(removals(&items).is_empty(), "{items:?}");
    assert_eq!(pids(&b).await, ["S", "S/a.cbz", "S/new.cbz", "T"]);
    std::fs::set_permissions(lib.abs("T"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // Readable again: nothing changed below it.
    let (items, _) = page(&b, &c1).await;
    assert!(items.is_empty(), "{items:?}");
}

#[tokio::test]
async fn leftovers_of_interrupted_writes_are_swept_at_connect() {
    let lib = Lib::new();
    lib.put("S/.bunko-00000000000000aa.tmp", b"old staged");
    lib.put("S/.bunko-00000000000000bb.tmp", b"fresh staged");
    lib.put(
        "S/.bunko-case-00000000000000cc.tmp",
        b"the only name a file has",
    );
    lib.mkdir(".bunko-probe-old");
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 86_400);
    for old in [
        "S/.bunko-00000000000000aa.tmp",
        "S/.bunko-case-00000000000000cc.tmp",
    ] {
        std::fs::File::options()
            .write(true)
            .open(lib.abs(old))
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }
    std::fs::File::open(lib.abs(".bunko-probe-old"))
        .and_then(|f| f.set_modified(two_days_ago))
        .ok();
    let _b = lib.open_watching().await;
    let t0 = Instant::now();
    while lib.abs("S/.bunko-00000000000000aa.tmp").exists() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the old staged file stayed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(lib.abs("S/.bunko-00000000000000bb.tmp").exists());
    assert!(lib.abs("S/.bunko-case-00000000000000cc.tmp").exists());
}

// ---- 10. wait_change -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_change_wakes_on_a_write_and_otherwise_at_max() {
    let lib = Lib::new();
    lib.put("S/x.cbz", b"x");
    let b = lib.open_watching().await;
    if !b.probe().watcher {
        eprintln!("no watcher on this host: only the polling path is checked");
    }
    let c = b.start_cursor().await.unwrap();
    // No events: back at max.
    let t0 = Instant::now();
    b.wait_change(&c, Duration::from_millis(800)).await.unwrap();
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(700), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    if b.probe().watcher {
        // A write: back after the quiet period, long before max.
        let w = {
            let root = lib.root.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::fs::write(root.join("S/y.cbz"), b"y").unwrap();
                Instant::now()
            })
        };
        let t0 = Instant::now();
        b.wait_change(&c, Duration::from_secs(60)).await.unwrap();
        let wrote = w.await.unwrap();
        let after = wrote.elapsed();
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
        assert!(after < Duration::from_secs(6), "{after:?} after the write");
        // The cursor of the walk that saw it does not wake again.
        let (_, c2) = page(&b, &c).await;
        let t0 = Instant::now();
        b.wait_change(&c2, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(400));
        // Bunko's own temporary names and Chromium's swap files wake nothing.
        let t0 = Instant::now();
        std::fs::write(lib.abs("S/z.cbz.crswap"), b"half").unwrap();
        std::fs::write(lib.abs("S/.bunko-0000000000000000.tmp"), b"staged").unwrap();
        b.wait_change(&c2, Duration::from_millis(1500))
            .await
            .unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(1400),
            "{:?}",
            t0.elapsed()
        );
    }
    // Without a watcher (a connect Check): back at max, never an error.
    let quiet = lib.open().await;
    let c = quiet.start_cursor().await.unwrap();
    let t0 = Instant::now();
    quiet
        .wait_change(&c, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(250));
}

// ---- 11. the trash ---------------------------------------------------------------------

/// Mokuro Reader's `isSyncableFile` (src/lib/util/sync/syncable-file.ts): a basename
/// and suffix rule.
fn reader_syncable(path: &str) -> bool {
    let base = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_lowercase();
    base.ends_with(".cbz")
        || base.ends_with(".mokuro")
        || base.ends_with(".mokuro.gz")
        || base.ends_with(".webp")
        || base.ends_with(".jpg")
        || base.ends_with(".jpeg")
        || base == "series.json"
        || [
            "volume-data.json",
            "profiles.json",
            "goals.json",
            "catalog.json",
        ]
        .contains(&base.as_str())
}

#[tokio::test]
async fn trashed_files_are_invisible_to_readers_and_recoverable() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"vol one");
    lib.put("S/Vol 1.mokuro", b"{\"pages\":[]}");
    lib.put("S/series.json", b"{}");
    lib.put("T/Vol 2.cbz", b"vol two");
    lib.put("T/cover.webp", b"img");
    lib.put("catalog.json", b"{}");
    let b = lib.open().await;
    b.trash("S/Vol 1.mokuro", None).await.unwrap();
    b.trash("catalog.json", None).await.unwrap();
    b.trash("T", None).await.unwrap();
    assert_eq!(pids(&b).await, ["S", "S/Vol 1.cbz", "S/series.json"]);
    let trash = lib.root.join(".mokuro-bunko-trash");
    let all = walk(&trash);
    let syncable: Vec<_> = all
        .iter()
        .filter(|p| p.is_file() && reader_syncable(&p.to_string_lossy()))
        .collect();
    assert!(syncable.is_empty(), "{syncable:?}");
    // By hand: every byte is still there, under the batch's stamp.
    let find = |suffix: &str| {
        all.iter()
            .find(|p| p.to_string_lossy().ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} in {all:?}"))
            .clone()
    };
    assert_eq!(
        std::fs::read(find("Vol 1.mokuro.bunko-trashed")).unwrap(),
        b"{\"pages\":[]}"
    );
    assert_eq!(
        std::fs::read(find("Vol 2.cbz.bunko-trashed")).unwrap(),
        b"vol two"
    );
    assert_eq!(
        std::fs::read(find("cover.webp.bunko-trashed")).unwrap(),
        b"img"
    );
    assert_eq!(
        std::fs::read(find("catalog.json.bunko-trashed")).unwrap(),
        b"{}"
    );
    // Old batches go at the next connect of a watching store; fresh ones stay.
    let old = trash.join("20200101T000000Z-deadbeef");
    std::fs::create_dir_all(old.join("S")).unwrap();
    std::fs::write(old.join("S/a.cbz.bunko-trashed"), b"old").unwrap();
    let fresh: Vec<PathBuf> = std::fs::read_dir(&trash)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| *p != old)
        .collect();
    assert_eq!(fresh.len(), 3);
    drop(b);
    let _b = lib.open_watching().await;
    assert!(!old.exists());
    for f in &fresh {
        assert!(f.exists(), "{f:?}");
    }
}

/// Every file below the trash folder whose name ends with `suffix`, with its bytes.
fn trashed(lib: &Lib, suffix: &str) -> Vec<Vec<u8>> {
    let trash = lib.root.join(".mokuro-bunko-trash");
    if !trash.exists() {
        return Vec::new();
    }
    walk(&trash)
        .into_iter()
        .filter(|p| p.is_file() && p.to_string_lossy().ends_with(suffix))
        .map(|p| std::fs::read(p).unwrap())
        .collect()
}

/// The engine deletes permanently only a version it judged Bunko's own; a reader that
/// put its own bytes there between that judgment and the delete keeps them, in the
/// trash. A file Bunko never wrote is the engine's call alone, as before.
#[tokio::test]
async fn a_permanent_delete_never_takes_a_readers_version() {
    let lib = Lib::new();
    lib.mkdir("S");
    let b = lib.open().await;
    let own = upload_new(&lib, &b, "S", "Vol 1.mokuro", b"bunko's ocr")
        .await
        .unwrap();
    let judged = b.stat(&own.pid).await.unwrap().unwrap();
    assert_eq!(judged.cver, own.cver);
    // The reader saves its edit over it (Chrome renames its swap file into place).
    lib.put("S/Vol 1.mokuro.crswap", b"the reader's edited ocr");
    std::fs::rename(lib.abs("S/Vol 1.mokuro.crswap"), lib.abs("S/Vol 1.mokuro")).unwrap();
    b.delete_permanent(&own.pid).await.unwrap();
    assert_eq!(b.stat(&own.pid).await.unwrap(), None);
    assert_eq!(
        trashed(&lib, "Vol 1.mokuro.bunko-trashed"),
        [b"the reader's edited ocr".to_vec()]
    );
    // Bunko's own version goes for good.
    let own = upload_new(&lib, &b, "S", "Vol 2.mokuro", b"ocr two")
        .await
        .unwrap();
    b.delete_permanent(&own.pid).await.unwrap();
    assert!(!lib.abs("S/Vol 2.mokuro").exists());
    assert!(trashed(&lib, "Vol 2.mokuro.bunko-trashed").is_empty());
    // A file Bunko never wrote: no memo of it, the engine's judgment stands.
    lib.put("S/cover.webp", b"img");
    b.delete_permanent("S/cover.webp").await.unwrap();
    assert!(!lib.abs("S/cover.webp").exists());
}

/// A replace copies its bytes first, then renames them over the file: a reader that
/// wrote the file during the copy keeps its bytes, and the outbox gets the conflict.
#[tokio::test]
async fn a_replace_never_lands_on_a_write_made_during_its_copy() {
    let lib = Lib::new();
    lib.put("S/series.json", b"{\"v\":0}");
    let b = lib.open().await;
    let base = b.stat("S/series.json").await.unwrap().unwrap();
    let theirs = lib.abs("S/series.json");
    b.before_next_replace_rename(move || {
        std::fs::write(&theirs, b"{\"v\":\"the reader's\"}").unwrap();
    });
    assert_eq!(
        replace(&lib, &b, "S/series.json", b"{\"v\":1}").await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(
        std::fs::read(lib.abs("S/series.json")).unwrap(),
        b"{\"v\":\"the reader's\"}"
    );
    assert!(lib.temps().is_empty(), "{:?}", lib.temps());
    // An `if_cver` that is not the file's version any more is refused up front.
    let (path, sha256) = lib.stage(b"{\"v\":2}");
    let r = b
        .upload(
            UploadTarget::Replace {
                pid: "S/series.json",
                if_cver: Some(&base.cver),
            },
            UploadSource::File {
                path: &path,
                size: 7,
                sha256,
            },
            &mut op(),
        )
        .await;
    assert_eq!(r, Err(BErr::PreconditionFailed));
    // Undisturbed, the replace lands.
    let now = replace(&lib, &b, "S/series.json", b"{\"v\":3}")
        .await
        .unwrap();
    assert_ne!(now.cver, base.cver);
    assert_eq!(
        std::fs::read(lib.abs("S/series.json")).unwrap(),
        b"{\"v\":3}"
    );
}

/// A folder's trash renames its files in place, then moves the folder: when the move
/// fails, the files get their names back, so readers do not see the folder emptied
/// with nothing in the trash.
#[tokio::test]
async fn a_folder_trash_that_fails_leaves_the_folder_as_it_was() {
    let lib = Lib::new();
    lib.put("S/Vol 1.cbz", b"one");
    lib.put("S/sub/Vol 2.cbz", b"two");
    // Something not a folder where the trash goes: the batch cannot be made.
    lib.put(".mokuro-bunko-trash", b"not a folder");
    let b = lib.open().await;
    assert!(b.trash("S", None).await.is_err());
    assert_eq!(
        pids(&b).await,
        ["S", "S/Vol 1.cbz", "S/sub", "S/sub/Vol 2.cbz"]
    );
    assert_eq!(std::fs::read(lib.abs("S/sub/Vol 2.cbz")).unwrap(), b"two");
    let left: Vec<_> = walk(&lib.abs("S"))
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".bunko-trashed"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

// ---- 12. coarse modification times ------------------------------------------------------

#[tokio::test]
async fn same_size_replaces_inside_one_tick_get_new_versions() {
    let lib = Lib::new();
    lib.put("S/series.json", b"{\"v\":0}");
    let b = lib.open().await;
    b.force_mtime_granularity(1_000_000_000);
    let before = b.stat("S/series.json").await.unwrap().unwrap();
    let one = replace(&lib, &b, "S/series.json", b"{\"v\":1}")
        .await
        .unwrap();
    let two = replace(&lib, &b, "S/series.json", b"{\"v\":2}")
        .await
        .unwrap();
    assert_ne!(one.cver, before.cver);
    assert_ne!(two.cver, one.cver);
    let new = upload_new(&lib, &b, "S", "a.json", b"{}").await.unwrap();
    let mtime_ns: i64 = new.cver.split('-').next().unwrap().parse().unwrap();
    assert_eq!(mtime_ns % 1_000_000_000, 0, "{}", new.cver);
}

// ---- 13. find_named and Unicode ---------------------------------------------------------

#[tokio::test]
async fn find_named_compares_nfc_and_reports_the_disk_spelling() {
    const NFC: &str = "\u{30ac}.cbz"; // ガ
    const NFD: &str = "\u{30ab}\u{3099}.cbz";
    let lib = Lib::new();
    lib.put(&format!("S/{NFC}"), b"composed");
    let second = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lib.abs("S").join(NFD))
        .is_ok();
    let on_disk: Vec<String> = std::fs::read_dir(lib.abs("S"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    let b = lib.open().await;
    for asked in [NFC, NFD] {
        let (found, _) = b.find_named("S", asked).await.unwrap();
        let mut got: Vec<String> = found
            .iter()
            .map(|n| String::from_utf8(n.name_raw.clone()).unwrap())
            .collect();
        got.sort();
        let mut want = on_disk.clone();
        want.sort();
        assert_eq!(got, want, "asked {asked:?} (second create: {second})");
        for n in &found {
            assert_eq!(n.pid, format!("S/{}", String::from_utf8_lossy(&n.name_raw)));
        }
    }
    assert!(b.find_named("S", "other.cbz").await.unwrap().0.is_empty());
    assert_eq!(b.find_named("Nope", NFC).await, Err(BErr::NotFound));
    assert_eq!(
        b.find_named(&format!("S/{}", on_disk[0]), NFC).await,
        Err(BErr::NotFound)
    );
}

// ---- 14, 15. reads and quota --------------------------------------------------------------

#[tokio::test]
async fn ranged_reads_are_inclusive_and_quota_is_known() {
    let lib = Lib::new();
    lib.put("S/x.bin", b"0123456789");
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    lib.put("S/big.bin", &big);
    let b = lib.open().await;
    assert_eq!(read_all(&b, "S/x.bin", None).await.unwrap(), b"0123456789");
    assert_eq!(
        read_all(&b, "S/x.bin", Some((2, Some(5)))).await.unwrap(),
        b"2345"
    );
    assert_eq!(
        read_all(&b, "S/x.bin", Some((7, None))).await.unwrap(),
        b"789"
    );
    assert_eq!(
        read_all(&b, "S/x.bin", Some((0, Some(100)))).await.unwrap(),
        b"0123456789"
    );
    assert!(matches!(
        read_all(&b, "S/x.bin", Some((10, None))).await,
        Err(BErr::Fatal(_))
    ));
    assert_eq!(read_all(&b, "S/none", None).await, Err(BErr::NotFound));
    assert_eq!(read_all(&b, "S", None).await, Err(BErr::NotFound));
    assert_eq!(read_all(&b, "S/big.bin", None).await.unwrap(), big);
    assert_eq!(
        read_all(&b, "S/big.bin", Some((1_048_570, Some(2_097_160))))
            .await
            .unwrap(),
        big[1_048_570..=2_097_160]
    );
    let q = b.quota().await.unwrap().unwrap();
    assert!(q.total > 0 && q.available <= q.total, "{q:?}");
    assert_eq!(list_dir(&b, "S/x.bin").await, Err(BErr::NotFound));
    assert_eq!(list_dir(&b, "Nope").await, Err(BErr::NotFound));
    assert_eq!(list_dir(&b, "S").await.unwrap().len(), 2);
}

// ---- 16. the hash memo -------------------------------------------------------------------

#[tokio::test]
async fn bunkos_own_versions_carry_their_hash() {
    let lib = Lib::new();
    lib.mkdir("S");
    let b = lib.open().await;
    let bytes = b"ocr result".to_vec();
    let sha = hex::encode(Sha256::digest(&bytes));
    let made = upload_new(&lib, &b, "S", "v.mokuro", &bytes).await.unwrap();
    assert_eq!(made.hash, Some((HashKind::Sha256, sha.clone())));
    let (found, _) = b.find_named("S", "v.mokuro").await.unwrap();
    assert_eq!(found[0].hash, Some((HashKind::Sha256, sha.clone())));
    assert_eq!(
        b.stat("S/v.mokuro").await.unwrap().unwrap().hash,
        Some((HashKind::Sha256, sha.clone()))
    );
    // Never in listings.
    assert!(list_all(&b).await.iter().all(|n| n.hash.is_none()));
    // A reopened backend still knows it; a move keeps it.
    drop(b);
    let b = lib.open().await;
    let moved = b
        .update("S/v.mokuro", None, Some("w.mokuro"), None)
        .await
        .unwrap();
    assert_eq!(moved.hash, Some((HashKind::Sha256, sha.clone())));
    // Replaced outside: the hash no longer holds.
    std::thread::sleep(Duration::from_millis(5));
    std::fs::write(lib.abs("S/w.mokuro"), b"reader edit").unwrap();
    let (found, _) = b.find_named("S", "w.mokuro").await.unwrap();
    assert_eq!(found[0].hash, None);
}

// ---- 17. Windows ----------------------------------------------------------------------------

#[cfg(windows)]
#[tokio::test]
async fn windows_names_and_sharing_violations() {
    use std::os::windows::fs::OpenOptionsExt;
    let lib = Lib::new();
    lib.put("S/x.cbz", b"x");
    let b = lib.open().await;
    for bad in ["dot.", "space ", "CON", "nul.cbz", "a<b", "q?.cbz"] {
        assert_eq!(
            upload_new(&lib, &b, "S", bad, b"x").await,
            Err(BErr::InvalidName),
            "{bad:?}"
        );
        assert_eq!(
            b.mkdir("S", bad, &op()).await,
            Err(BErr::InvalidName),
            "{bad:?}"
        );
    }
    assert_eq!(pids(&b).await, ["S", "S/x.cbz"]);
    // Held open without FILE_SHARE_DELETE: a rename waits for it.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ
        .open(lib.abs("S/x.cbz"))
        .unwrap();
    let r = b.update("S/x.cbz", None, Some("y.cbz"), None).await;
    assert!(matches!(r, Err(BErr::Transient(_))), "{r:?}");
    drop(held);
    b.update("S/x.cbz", None, Some("y.cbz"), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn timestamps_are_epoch_based() {
    let lib = Lib::new();
    lib.put("x.cbz", b"x");
    let b = lib.open().await;
    let n = b.stat("x.cbz").await.unwrap().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!((now - n.base_mtime_ms).abs() < 60_000, "{n:?}");
    assert_eq!(n.version, n.cver);
    assert!(n.cver.ends_with("-1"), "{n:?}");
}
