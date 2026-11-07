//! `bunko_store::durable`: the no-replace move ladder, its refusals in every mode, its
//! crash points, folder fsync and file identity. Runs on every host CI has (Linux,
//! macOS, Windows) and on the casefold and plain tmpfs profiles.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bunko_store::durable::testing::{self, CrashPoint, Faults};
use bunko_store::fs::{
    MoveMode, copy_exclusive_verified, fsync_dir, identity, is_taken, move_noreplace, sha256_file,
};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Does this folder's disk fold case (macOS, Windows, a casefold tmpfs)?
fn folds_case(dir: &Path) -> bool {
    let probe = dir.join(".fold-probe-a");
    std::fs::write(&probe, b"").unwrap();
    let folds = dir.join(".FOLD-PROBE-A").exists();
    std::fs::remove_file(&probe).unwrap();
    folds
}

fn old_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(1_500_000_000, 123_400_000)
}

fn write_aged(p: &Path, bytes: &[u8]) {
    std::fs::write(p, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(old_time())
        .unwrap();
}

/// Every file under `root`, by relative path, with its bytes (folders as `<dir>/`).
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let t = e.file_type().unwrap();
            if t.is_dir() {
                out.insert(format!("{rel}/"), None);
                walk(root, &p, out);
            } else {
                out.insert(rel, std::fs::read(&p).ok());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The multiset of file contents under `root`, by sha256 (what "nothing is lost" checks).
fn contents(root: &Path) -> Vec<[u8; 32]> {
    let mut v: Vec<[u8; 32]> = tree(root)
        .into_iter()
        .filter(|(k, _)| !k.ends_with('/'))
        .map(|(k, _)| sha256_file(root.join(k)).unwrap())
        .collect();
    v.sort();
    v
}

/// A symbolic link to nothing in `dir` (Unix; Windows needs a privilege for one).
fn dangling_symlink(dir: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let link = dir.join("link.cbz");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).unwrap();
        Some(link)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

const FILE_MODES: [MoveMode; 3] = [MoveMode::Native, MoveMode::Link, MoveMode::Copy];

fn bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn modes_round_trip_their_names() {
    for m in [
        MoveMode::Native,
        MoveMode::Link,
        MoveMode::Claim,
        MoveMode::Copy,
    ] {
        assert_eq!(MoveMode::parse(m.as_str()), Some(m));
        assert_eq!(m.to_string(), m.as_str());
    }
    assert_eq!(MoveMode::parse("rename"), None);
}

/// The hosts CI runs on all have a no-replace rename, for files and folders.
#[test]
fn the_native_rename_moves_files_and_folders_here() {
    let d = tmp();
    let a = d.path().join("a.cbz");
    write_aged(&a, b"archive");
    let id = identity(&a).unwrap();
    let mode = move_noreplace(&a, d.path().join("b.cbz")).unwrap();
    if cfg!(any(target_os = "linux", target_vendor = "apple", windows)) {
        assert_eq!(mode, MoveMode::Native);
    }
    assert!(!a.exists());
    assert_eq!(
        identity(d.path().join("b.cbz")).unwrap(),
        id,
        "a rename keeps the file"
    );

    let s = d.path().join("Series");
    std::fs::create_dir_all(s.join("inner")).unwrap();
    std::fs::write(s.join("inner/x.mokuro"), b"x").unwrap();
    let mode = move_noreplace(&s, d.path().join("Other")).unwrap();
    if cfg!(any(target_os = "linux", target_vendor = "apple", windows)) {
        assert_eq!(mode, MoveMode::Native);
    }
    assert_eq!(
        std::fs::read(d.path().join("Other/inner/x.mokuro")).unwrap(),
        b"x"
    );
    assert!(!s.exists());
}

#[test]
fn every_file_mode_moves_the_bytes_and_keeps_the_mtime() {
    for mode in FILE_MODES {
        let d = tmp();
        let _g = testing::force(d.path(), mode);
        std::fs::create_dir(d.path().join("Kingdom")).unwrap();
        let from = d.path().join("kingdom.cbz");
        let data = bytes(3 * (1 << 20) + 17);
        write_aged(&from, &data);
        let before = identity(&from).unwrap();
        let to = d.path().join("Kingdom/Vol 1.cbz");
        assert_eq!(move_noreplace(&from, &to).unwrap(), mode, "{mode}");
        assert!(!from.exists(), "{mode}");
        assert_eq!(std::fs::read(&to).unwrap(), data, "{mode}");
        assert_eq!(
            std::fs::metadata(&to).unwrap().modified().unwrap(),
            old_time(),
            "{mode}"
        );
        let after = identity(&to).unwrap();
        match mode {
            MoveMode::Copy => assert_ne!(after, before, "a copy is a new file"),
            _ => assert_eq!(after, before, "{mode} keeps the file"),
        }
    }
}

/// Anything at the destination refuses the move, in every mode, and leaves both names
/// as they were: a file, an empty folder, a folder with files, a symbolic link.
#[test]
fn every_file_mode_refuses_a_taken_destination() {
    for mode in FILE_MODES {
        let d = tmp();
        let _g = testing::force(d.path(), mode);
        let from = d.path().join("loser.cbz");
        write_aged(&from, b"loser");
        let file = d.path().join("file.cbz");
        std::fs::write(&file, b"winner").unwrap();
        let empty = d.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let full = d.path().join("full");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("x"), b"x").unwrap();
        let taken: Vec<PathBuf> = [file, empty, full]
            .into_iter()
            .chain(dangling_symlink(d.path()))
            .collect();
        let before = tree(d.path());
        for to in &taken {
            let e = move_noreplace(&from, to).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{mode} {to:?}: {e}");
            assert!(is_taken(&e));
            assert_eq!(tree(d.path()), before, "{mode} {to:?}: nothing changed");
        }
        // The same name: there is nothing to move.
        let e = move_noreplace(&from, &from).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(tree(d.path()), before);
    }
}

#[test]
fn folders_move_natively_or_by_claim_and_refuse_a_taken_destination() {
    for mode in [MoveMode::Native, MoveMode::Claim] {
        let d = tmp();
        let _g = testing::force(d.path(), mode);
        let from = d.path().join("kingdom");
        std::fs::create_dir_all(from.join("deep")).unwrap();
        std::fs::write(from.join("Vol 1.cbz"), b"v1").unwrap();
        std::fs::write(from.join("deep/x.mokuro"), b"x").unwrap();
        let empty = d.path().join("Empty");
        std::fs::create_dir(&empty).unwrap();
        let full = d.path().join("Full");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("Vol 2.cbz"), b"v2").unwrap();
        let file = d.path().join("File");
        std::fs::write(&file, b"f").unwrap();
        let before = tree(d.path());
        for to in [&empty, &full, &file] {
            let e = move_noreplace(&from, to).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{mode} {to:?}: {e}");
            assert_eq!(tree(d.path()), before, "{mode} {to:?}: nothing changed");
        }
        let to = d.path().join("Kingdom 2");
        let id = identity(&from).unwrap();
        match move_noreplace(&from, &to) {
            Ok(m) => {
                assert_eq!(m, mode);
                assert_eq!(identity(&to).unwrap(), id, "the folder itself moved");
                assert_eq!(std::fs::read(to.join("deep/x.mokuro")).unwrap(), b"x");
                assert!(!from.exists());
            }
            // Windows has no claim rung (its native move takes folders).
            Err(e) if cfg!(windows) && mode == MoveMode::Claim => {
                assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
                assert_eq!(tree(d.path()), before);
            }
            Err(e) => panic!("{mode}: {e}"),
        }
    }
}

/// A claim that loses its race (something lands in the claimed folder) refuses the move
/// and leaves what landed there.
#[cfg(unix)]
#[test]
fn a_claim_never_replaces_what_lands_in_it() {
    let d = tmp();
    let from = d.path().join("loser");
    std::fs::create_dir(&from).unwrap();
    std::fs::write(from.join("a"), b"a").unwrap();
    let to = d.path().join("winner");
    {
        let _g = testing::inject(
            d.path(),
            Faults {
                native_unsupported: true,
                crash: Some(CrashPoint::Claimed),
                ..Faults::default()
            },
        );
        let e = move_noreplace(&from, &to).unwrap_err();
        assert!(testing::is_crash(&e), "{e}");
    }
    // The crash left the claimed (empty) folder: a new move refuses it.
    assert!(to.is_dir() && std::fs::read_dir(&to).unwrap().next().is_none());
    std::fs::write(to.join("b"), b"b").unwrap();
    let _g = testing::force(d.path(), MoveMode::Claim);
    let e = move_noreplace(&from, &to).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(to.join("b")).unwrap(), b"b");
    assert_eq!(std::fs::read(from.join("a")).unwrap(), b"a");
}

/// Two spellings of one name: a disk that folds case sees the destination taken (the
/// file itself) and refuses; a case-sensitive one moves. Never a second spelling is
/// replaced, in any mode.
#[test]
fn a_case_only_move_is_refused_exactly_where_the_disk_folds() {
    for mode in FILE_MODES {
        let d = tmp();
        let folds = folds_case(d.path());
        let _g = testing::force(d.path(), mode);
        let from = d.path().join("Vol 1.cbz");
        std::fs::write(&from, b"one").unwrap();
        let to = d.path().join("VOL 1.cbz");
        match move_noreplace(&from, &to) {
            Err(e) if folds => {
                assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{mode}: {e}");
                let names: Vec<String> = std::fs::read_dir(d.path())
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, ["Vol 1.cbz"], "{mode}: the spelling is kept");
            }
            Ok(m) if !folds => {
                assert_eq!(m, mode);
                assert!(!from.exists());
                assert_eq!(std::fs::read(&to).unwrap(), b"one");
            }
            other => panic!("{mode}: folds={folds}: {other:?}"),
        }
    }
}

#[test]
fn across_filesystems_a_file_is_copied_and_a_folder_refused() {
    let d = tmp();
    let _g = testing::inject(
        d.path(),
        Faults {
            cross_device: true,
            ..Faults::default()
        },
    );
    let from = d.path().join("a.cbz");
    write_aged(&from, b"bytes");
    let to = d.path().join("b.cbz");
    assert_eq!(move_noreplace(&from, &to).unwrap(), MoveMode::Copy);
    assert!(!from.exists());
    assert_eq!(std::fs::read(&to).unwrap(), b"bytes");
    assert_eq!(
        std::fs::metadata(&to).unwrap().modified().unwrap(),
        old_time()
    );

    let dir = d.path().join("Series");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("x"), b"x").unwrap();
    let before = tree(d.path());
    let e = move_noreplace(&dir, d.path().join("Other")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::CrossesDevices, "{e}");
    assert_eq!(tree(d.path()), before);
}

#[test]
fn refused_links_fall_to_the_copy() {
    let d = tmp();
    let _g = testing::inject(
        d.path(),
        Faults {
            native_unsupported: true,
            link_refused: true,
            ..Faults::default()
        },
    );
    let from = d.path().join("a.mokuro");
    std::fs::write(&from, b"{}").unwrap();
    let to = d.path().join("b.mokuro");
    assert_eq!(move_noreplace(&from, &to).unwrap(), MoveMode::Copy);
    assert_eq!(std::fs::read(&to).unwrap(), b"{}");
    assert!(!from.exists());
}

/// The newest injection under the longest root wins; a dropped guard ends it.
#[test]
fn injections_nest_and_end_with_their_guard() {
    let d = tmp();
    let inner = d.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let outer_guard = testing::force(d.path(), MoveMode::Copy);
    {
        let _inner = testing::force(&inner, MoveMode::Link);
        std::fs::write(inner.join("a"), b"a").unwrap();
        assert_eq!(
            move_noreplace(inner.join("a"), inner.join("b")).unwrap(),
            MoveMode::Link
        );
    }
    std::fs::write(inner.join("c"), b"c").unwrap();
    assert_eq!(
        move_noreplace(inner.join("c"), inner.join("d")).unwrap(),
        MoveMode::Copy
    );
    drop(outer_guard);
    std::fs::write(inner.join("e"), b"e").unwrap();
    let m = move_noreplace(inner.join("e"), inner.join("f")).unwrap();
    if cfg!(any(target_os = "linux", target_vendor = "apple", windows)) {
        assert_eq!(m, MoveMode::Native);
    }
}

#[test]
fn a_missing_source_or_folder_is_an_error_and_creates_nothing() {
    for mode in FILE_MODES {
        let d = tmp();
        let _g = testing::force(d.path(), mode);
        let e = move_noreplace(d.path().join("none"), d.path().join("x")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{mode}");
        std::fs::write(d.path().join("a"), b"a").unwrap();
        let before = tree(d.path());
        assert!(move_noreplace(d.path().join("a"), d.path().join("no/such/dir/a")).is_err());
        assert_eq!(tree(d.path()), before, "{mode}");
    }
}

/// A crash inside a move leaves a state the journal can decide from the listing and
/// identities alone, never a lost file: the source intact (with or without a partial or
/// complete copy of another identity beside it), or both names of one file (link).
#[test]
fn every_crash_point_leaves_the_bytes_and_a_decidable_state() {
    let data = bytes(2 * (1 << 20) + 5);
    for (mode, at) in [
        (MoveMode::Link, CrashPoint::Linked),
        (MoveMode::Copy, CrashPoint::CopyHalfway),
        (MoveMode::Copy, CrashPoint::Copied),
    ] {
        let d = tmp();
        let from = d.path().join("loser.cbz");
        write_aged(&from, &data);
        let src_id = identity(&from).unwrap();
        let want = sha256_file(&from).unwrap();
        let to = d.path().join("winner.cbz");
        {
            let _g = testing::inject(
                d.path(),
                Faults {
                    native_unsupported: true,
                    link_refused: mode == MoveMode::Copy,
                    crash: Some(at),
                    ..Faults::default()
                },
            );
            let e = move_noreplace(&from, &to).unwrap_err();
            assert!(testing::is_crash(&e), "{at:?}: {e}");
        }
        // The source is intact in every case.
        assert_eq!(identity(&from).unwrap(), src_id, "{at:?}");
        assert_eq!(sha256_file(&from).unwrap(), want, "{at:?}");
        let dest_id = identity(&to).unwrap();
        match at {
            CrashPoint::Linked => assert_eq!(dest_id, src_id, "both names, one file"),
            CrashPoint::CopyHalfway => {
                assert_ne!(dest_id, src_id);
                assert!(std::fs::metadata(&to).unwrap().len() < data.len() as u64);
                assert_ne!(sha256_file(&to).unwrap(), want, "a partial copy");
            }
            CrashPoint::Copied => {
                assert_ne!(dest_id, src_id);
                assert_eq!(sha256_file(&to).unwrap(), want, "a complete copy");
            }
            CrashPoint::Claimed => unreachable!(),
        }
        // A rerun never moves over what the crash left.
        let e = move_noreplace(&from, &to).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{at:?}");
        assert_eq!(sha256_file(&from).unwrap(), want, "{at:?}");
        // What a recovery does: the same file twice drops the source name; a copy is
        // kept only when complete (then the source goes); a partial one is redone.
        match at {
            CrashPoint::Linked | CrashPoint::Copied => std::fs::remove_file(&from).unwrap(),
            _ => {
                std::fs::remove_file(&to).unwrap();
                assert_eq!(move_noreplace(&from, &to).unwrap(), MoveMode::Native);
            }
        }
        assert_eq!(sha256_file(&to).unwrap(), want, "{at:?}");
        assert_eq!(
            contents(d.path()),
            vec![want],
            "{at:?}: exactly one copy left"
        );
    }
}

#[test]
fn an_exclusive_copy_is_verified_keeps_the_source_and_refuses_a_taken_name() {
    let d = tmp();
    let from = d.path().join("a.cbz");
    let data = bytes(1 << 20);
    write_aged(&from, &data);
    let to = d.path().join("keep/a.cbz");
    std::fs::create_dir(d.path().join("keep")).unwrap();
    let sum = copy_exclusive_verified(&from, &to).unwrap();
    assert_eq!(sum, sha256_file(&from).unwrap());
    assert_eq!(std::fs::read(&from).unwrap(), data, "the source stays");
    assert_eq!(std::fs::read(&to).unwrap(), data);
    assert_eq!(
        std::fs::metadata(&to).unwrap().modified().unwrap(),
        old_time()
    );
    // Taken: refused, and the copy already there is not touched.
    std::fs::write(&to, b"other").unwrap();
    let e = copy_exclusive_verified(&from, &to).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&to).unwrap(), b"other");
    // A missing source creates nothing.
    let none = d.path().join("keep/none");
    assert_eq!(
        copy_exclusive_verified(d.path().join("missing"), &none)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!none.exists());
    // A folder is not a file to copy.
    assert!(copy_exclusive_verified(d.path().join("keep"), d.path().join("k2")).is_err());
    assert!(!d.path().join("k2").exists());
}

#[cfg(unix)]
#[test]
fn a_copy_keeps_the_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp();
    let from = d.path().join("a");
    std::fs::write(&from, b"a").unwrap();
    std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o640)).unwrap();
    let _g = testing::force(d.path(), MoveMode::Copy);
    let to = d.path().join("b");
    assert_eq!(move_noreplace(&from, &to).unwrap(), MoveMode::Copy);
    assert_eq!(
        std::fs::metadata(&to).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn fsync_dir_syncs_a_folder_and_refuses_what_is_not_one() {
    let d = tmp();
    std::fs::write(d.path().join("f"), b"f").unwrap();
    fsync_dir(d.path()).unwrap();
    assert_eq!(
        fsync_dir(d.path().join("missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(fsync_dir(d.path().join("f")).is_err());
}

#[test]
fn identity_tells_files_apart_and_links_together() {
    let d = tmp();
    let a = d.path().join("a");
    let b = d.path().join("b");
    std::fs::write(&a, b"same").unwrap();
    std::fs::write(&b, b"same").unwrap();
    assert_ne!(identity(&a).unwrap(), identity(&b).unwrap());
    let c = d.path().join("c");
    std::fs::hard_link(&a, &c).unwrap();
    assert_eq!(identity(&a).unwrap(), identity(&c).unwrap());
    let f = std::fs::File::open(&a).unwrap();
    assert_eq!(
        bunko_store::fs::file_identity(&f).unwrap(),
        identity(&a).unwrap()
    );
    assert_eq!(identity(d.path()).unwrap().dev, identity(&a).unwrap().dev);
    assert_eq!(
        identity(d.path().join("none")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!identity(&a).unwrap().to_string().is_empty());
}

/// A symbolic link is itself, not its target: its identity is its own, it moves as the
/// link by the native rename, and no other rung copies its target's bytes in its place.
#[cfg(unix)]
#[test]
fn a_symlink_moves_only_as_itself() {
    let d = tmp();
    let target = d.path().join("target.cbz");
    std::fs::write(&target, b"t").unwrap();
    let link = d.path().join("link.cbz");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_ne!(identity(&link).unwrap(), identity(&target).unwrap());
    let moved = d.path().join("moved.cbz");
    assert_eq!(move_noreplace(&link, &moved).unwrap(), MoveMode::Native);
    assert_eq!(std::fs::read_link(&moved).unwrap(), target);
    for mode in [MoveMode::Link, MoveMode::Copy] {
        let _g = testing::force(d.path(), mode);
        let before = tree(d.path());
        let e = move_noreplace(&moved, d.path().join("again.cbz")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{mode}: {e}");
        assert_eq!(tree(d.path()), before);
        assert!(std::fs::symlink_metadata(&moved).unwrap().is_symlink());
    }
}

mod remote {
    use super::*;
    use bunko_store::{
        Caps, File, LibraryStore, LocalPath, OpenOptions, Published, ReadDir, Version, WatchGuard,
        WatchSink, WriteIntent, meta::Meta,
    };

    pub struct Remote(pub PathBuf);

    fn no() -> io::Error {
        io::ErrorKind::Unsupported.into()
    }

    impl LibraryStore for Remote {
        fn root(&self) -> &Path {
            &self.0
        }
        fn caps(&self) -> Caps {
            Caps {
                remote: true,
                ..Caps::default()
            }
        }
        fn metadata(&self, _: &Path) -> io::Result<Meta> {
            Err(no())
        }
        fn symlink_metadata(&self, _: &Path) -> io::Result<Meta> {
            Err(no())
        }
        fn read_dir(&self, _: &Path) -> io::Result<ReadDir> {
            Err(no())
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            Ok(p.into())
        }
        fn read_link(&self, _: &Path) -> io::Result<PathBuf> {
            Err(no())
        }
        fn open(&self, _: &Path) -> io::Result<File> {
            Err(no())
        }
        fn local_path(&self, _: &Path, _: Option<&Version>) -> io::Result<LocalPath> {
            Err(no())
        }
        fn open_with(&self, _: &Path, _: &OpenOptions) -> io::Result<File> {
            Err(no())
        }
        fn publish(&self, _: &Path, _: &Path, _: &WriteIntent) -> io::Result<Published> {
            Err(no())
        }
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn copy(&self, _: &Path, _: &Path) -> io::Result<u64> {
            Err(no())
        }
        fn remove_file(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn remove_dir(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn create_dir(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn create_dir_all(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn watch(&self, _: WatchSink) -> io::Result<WatchGuard> {
            Err(no())
        }
    }
}

/// A path under a remote store is not a local disk: each call refuses it.
#[test]
fn remote_store_paths_are_refused() {
    let d = tmp();
    let root = d.path().join("cloud");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a"), b"a").unwrap();
    let local = d.path().join("local");
    std::fs::write(&local, b"l").unwrap();
    let _m = bunko_store::mount(Arc::new(remote::Remote(root.clone())));
    let unsupported = |r: io::Result<()>| {
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::Unsupported);
    };
    unsupported(move_noreplace(root.join("a"), root.join("b")).map(drop));
    unsupported(move_noreplace(&local, root.join("l")).map(drop));
    unsupported(copy_exclusive_verified(root.join("a"), d.path().join("c")).map(drop));
    unsupported(fsync_dir(&root));
    unsupported(identity(root.join("a")).map(drop));
    assert!(local.exists() && !d.path().join("c").exists());
}
