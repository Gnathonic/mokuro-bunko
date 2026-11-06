use std::io::{Read, Write};
use std::sync::Arc;

use bunko_store::fs;
use bunko_store::{Durability, LocalStore, Mode, Origin, WriteIntent, mount, store_for};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn unmounted_paths_use_std() {
    let d = tmp();
    let p = d.path().join("a.txt");
    fs::write(&p, b"hi").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"hi");
    assert!(store_for(&p).is_none());
    assert!(fs::is_file(&p));
    assert!(fs::exists(&p));
    assert!(!fs::is_dir(&p));
}

#[test]
fn meta_std_matches_std_values() {
    let d = tmp();
    let p = d.path().join("old.bin");
    std::fs::write(&p, b"12345").unwrap();
    // pre-1970 with nanoseconds
    let t = std::time::UNIX_EPOCH - std::time::Duration::new(86_400 * 400, 123_456_789);
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(t)
        .unwrap();
    let ours = fs::metadata(&p).unwrap();
    let std_meta = std::fs::metadata(&p).unwrap();
    assert_eq!(ours.len(), std_meta.len());
    assert_eq!(ours.modified().unwrap(), std_meta.modified().unwrap());
    assert!(ours.is_file() && !ours.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            ours.unix_mtime(),
            Some((std_meta.mtime(), std_meta.mtime_nsec()))
        );
        assert_eq!(ours.dev(), Some(std_meta.dev()));
        assert_eq!(ours.ino(), Some(std_meta.ino()));
    }
}

#[test]
fn longest_prefix_mount_wins_and_guard_unmounts() {
    let d = tmp();
    let outer = d.path().join("lib");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let g1 = mount(Arc::new(LocalStore::new(&outer)));
    let g2 = mount(Arc::new(LocalStore::new(&inner)));
    assert_eq!(store_for(&inner.join("x")).unwrap().root(), inner.as_path());
    assert_eq!(store_for(&outer.join("y")).unwrap().root(), outer.as_path());
    drop(g2);
    assert_eq!(store_for(&inner.join("x")).unwrap().root(), outer.as_path());
    drop(g1);
    assert!(store_for(&outer.join("y")).is_none());
}

#[test]
fn local_store_round_trip_through_shim() {
    let d = tmp();
    let root = d.path().join("library");
    std::fs::create_dir_all(&root).unwrap();
    let _g = mount(Arc::new(LocalStore::new(&root)));
    let series = root.join("Kingdom");
    fs::create_dir_all(&series).unwrap();
    let staged = series.join(".v1.cbz.upload-x.tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .unwrap();
    f.write_all(b"zipbytes").unwrap();
    f.sync_all().unwrap();
    drop(f);
    let dest = series.join("v1.cbz");
    let intent = WriteIntent {
        mode: Mode::CreateNew,
        origin: Origin::Admin,
        durability: Durability::Sync,
    };
    fs::publish(&staged, &dest, &intent).unwrap();
    assert!(!staged.exists());
    let mut s = String::new();
    fs::File::open(&dest)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "zipbytes");
    let names: Vec<_> = fs::read_dir(&series)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["v1.cbz".to_string()]);
    let e = fs::read_dir(&series).unwrap().next().unwrap().unwrap();
    assert!(e.file_type().unwrap().is_file());
    assert_eq!(e.metadata().unwrap().len(), 8);
    fs::remove_file(&dest).unwrap();
    fs::remove_dir(&series).unwrap();
    assert!(!fs::exists(&series));
}

#[test]
fn rename_from_unmounted_into_local_store_is_std_rename() {
    let d = tmp();
    let root = d.path().join("library");
    let outside = d.path().join(".processing");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let _g = mount(Arc::new(LocalStore::new(&root)));
    let src = outside.join("result.mokuro");
    std::fs::write(&src, b"{}").unwrap();
    let dst = root.join("v.mokuro");
    fs::rename(&src, &dst).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"{}");
    assert!(!src.exists());
}

#[test]
fn read_dir_entry_vanishing_is_per_entry_error() {
    let d = tmp();
    std::fs::write(d.path().join("a"), b"").unwrap();
    std::fs::write(d.path().join("b"), b"").unwrap();
    let entries: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    std::fs::remove_file(d.path().join("a")).unwrap();
    let results: Vec<_> = entries.iter().map(|e| e.metadata().is_ok()).collect();
    // Windows answers an entry's metadata from the listing itself (FindNextFile), so
    // the vanished entry still has some; elsewhere it is a fresh stat that fails alone.
    let expected = if cfg!(windows) { 2 } else { 1 };
    assert_eq!(results.iter().filter(|ok| **ok).count(), expected);
}

#[test]
fn copy_and_canonicalize_pass_through() {
    let d = tmp();
    let a = d.path().join("a");
    std::fs::write(&a, b"xyz").unwrap();
    let b = d.path().join("b");
    assert_eq!(fs::copy(&a, &b).unwrap(), 3);
    assert_eq!(
        fs::canonicalize(&b).unwrap(),
        std::fs::canonicalize(&b).unwrap()
    );
    assert_eq!(fs::read_to_string(&b).unwrap(), "xyz");
}

/// Names in a folder, sorted.
fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A case-only rename takes effect on every host: POSIX `rename(2)` between two names
/// of one file does nothing (Linux casefold), APFS and NTFS rename. The listing holds
/// exactly the new spelling afterwards, with no temporary name left behind.
#[test]
fn a_case_only_rename_takes_effect() {
    let d = tmp();
    std::fs::write(d.path().join("a.txt"), b"body").unwrap();
    fs::rename(d.path().join("a.txt"), d.path().join("A.txt")).unwrap();
    assert_eq!(names(d.path()), ["A.txt"]);
    assert_eq!(std::fs::read(d.path().join("A.txt")).unwrap(), b"body");

    let s = d.path().join("s");
    std::fs::create_dir(&s).unwrap();
    std::fs::write(s.join("v.cbz"), b"zip").unwrap();
    fs::rename(&s, d.path().join("S")).unwrap();
    assert_eq!(names(d.path()), ["A.txt", "S"]);
    assert_eq!(names(&d.path().join("S")), ["v.cbz"]);
    assert_eq!(
        std::fs::read(d.path().join("S").join("v.cbz")).unwrap(),
        b"zip"
    );
}

/// The same through a mounted [`LocalStore`] and through the tokio shim.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn a_case_only_rename_takes_effect_in_a_store() {
    let d = tmp();
    let root = d.path().join("lib");
    std::fs::create_dir(&root).unwrap();
    let _g = mount(Arc::new(LocalStore::new(&root)));
    std::fs::write(root.join("v.cbz"), b"zip").unwrap();
    fs::rename(root.join("v.cbz"), root.join("V.cbz")).unwrap();
    assert_eq!(names(&root), ["V.cbz"]);
    bunko_store::afs::rename(root.join("V.cbz"), root.join("V.CBZ"))
        .await
        .unwrap();
    assert_eq!(names(&root), ["V.CBZ"]);
    assert_eq!(std::fs::read(root.join("V.CBZ")).unwrap(), b"zip");
}

#[test]
fn same_file_tells_one_entry_from_two() {
    let d = tmp();
    let a = d.path().join("a");
    std::fs::write(&a, b"x").unwrap();
    std::fs::write(d.path().join("b"), b"x").unwrap();
    assert!(fs::same_file(&a, &a));
    assert!(!fs::same_file(&a, &d.path().join("b")));
    assert!(!fs::same_file(&a, &d.path().join("missing")));
}

/// A case-only rename of a name near the 255-byte limit still takes effect: the
/// temporary name never carries more than the file system takes.
#[test]
fn a_case_only_rename_of_a_long_name_takes_effect() {
    let d = tmp();
    // 78 three-byte characters plus `v.cbz`: 239 bytes.
    let stem = "巻".repeat(78);
    let from = d.path().join(format!("{stem}v.cbz"));
    let to = d.path().join(format!("{stem}V.cbz"));
    std::fs::write(&from, b"zip").unwrap();
    fs::rename(&from, &to).unwrap();
    assert_eq!(names(d.path()), [format!("{stem}V.cbz")]);
    assert_eq!(std::fs::read(&to).unwrap(), b"zip");
}

/// Two hard links of one file end as the one name renamed to, with no temporary name
/// left behind (rename semantics: the source name goes).
#[cfg(unix)]
#[test]
fn a_rename_between_two_hard_links_leaves_one_name() {
    let d = tmp();
    let a = d.path().join("a.cbz");
    let b = d.path().join("b.cbz");
    std::fs::write(&a, b"zip").unwrap();
    std::fs::hard_link(&a, &b).unwrap();
    fs::rename(&a, &b).unwrap();
    assert_eq!(names(d.path()), ["b.cbz"]);
    assert_eq!(std::fs::read(&b).unwrap(), b"zip");
}
