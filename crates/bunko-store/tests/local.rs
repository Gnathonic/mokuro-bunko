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
