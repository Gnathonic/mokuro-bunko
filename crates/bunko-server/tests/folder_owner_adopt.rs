//! The series folders that existed before 0.8 get their owners once, at startup: the
//! one uploader of a folder's tracked volumes, when no archive in it is untracked.
//! Every other folder (mixed, untracked, empty) stays shared.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

use bunko_db::{Database, DbOptions};
use bunko_server::library::{FolderAdoption, adopt_series_folder_owners};

struct Lib {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: Database,
}

impl Lib {
    /// A library and a database that existed before 0.8: adoption is pending.
    fn new() -> Lib {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();
        let path = dir.path().join("mokuro.db");
        let db = Database::open_with(&path, &DbOptions::default()).unwrap();
        db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
            .unwrap();
        assert!(db.series_folder_adoption_pending().unwrap());
        Lib {
            _dir: dir,
            root,
            db,
        }
    }

    fn file(&self, rel: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// An archive `user` uploaded.
    fn upload(&self, rel: &str, user: &str) {
        self.file(rel);
        self.db.record_volume_upload(rel, user).unwrap();
    }

    fn owner(&self, folder: &str) -> Option<String> {
        self.db.series_folder_owner(folder).unwrap()
    }

    fn adopt(&self) -> Option<FolderAdoption> {
        adopt_series_folder_owners(&self.db, &self.root)
    }
}

#[test]
fn a_folder_of_one_uploader_becomes_its() {
    let lib = Lib::new();
    lib.upload("Solo/Vol 1.cbz", "alice");
    lib.upload("Solo/Vol 2.cbz", "ALICE");
    lib.file("Solo/Vol 1.mokuro");
    lib.file("Solo/Vol 1.webp");
    lib.upload("Nested/Part A/Vol 1.cbz", "bob");
    lib.upload("Mixed/Vol 1.cbz", "alice");
    lib.upload("Mixed/Vol 2.cbz", "bob");
    lib.upload("Untracked/Vol 1.cbz", "alice");
    lib.file("Untracked/Vol 2.cbz");
    lib.upload("Zipped/Vol 1.cbz", "alice");
    lib.file("Zipped/Vol 2.zip");
    std::fs::create_dir_all(lib.root.join("Empty")).unwrap();
    lib.file("Loose/notes.txt");
    lib.upload("loose.cbz", "carol");
    // The cloud-root owner's own tree is no series.
    lib.file("history/d/2026-10.events");

    assert_eq!(
        lib.adopt(),
        Some(FolderAdoption {
            owned: 2,
            unowned: 5
        })
    );
    assert_eq!(lib.owner("Solo").as_deref(), Some("alice"));
    assert_eq!(lib.owner("Nested").as_deref(), Some("bob"));
    for folder in ["Mixed", "Untracked", "Zipped", "Empty", "Loose", "history"] {
        assert_eq!(lib.owner(folder), None, "{folder}");
    }
    assert!(!lib.db.series_folder_adoption_pending().unwrap());

    // Once: a later start changes nothing, whatever was uploaded meanwhile.
    lib.upload("Empty/Vol 1.cbz", "carol");
    assert_eq!(lib.adopt(), None);
    assert_eq!(lib.owner("Empty"), None);
}

/// Folders whose names are one series key (`fold_series_title_key` folds case and
/// collapses whitespace) count together. The spellings differ in whitespace too, so they
/// are separate folders on every host: a case-only variant is one folder on APFS, NTFS
/// and casefold, and the server's PathCase never makes a second one anywhere.
#[test]
fn every_spelling_of_a_series_counts_together() {
    let lib = Lib::new();
    lib.upload("dr stone/Vol 1.cbz", "alice");
    lib.upload("Dr  Stone/Vol 2.cbz", "alice");
    lib.upload("Kingdom  Hearts/Vol 1.cbz", "alice");
    lib.upload("kingdom hearts/Vol 2.cbz", "bob");
    lib.upload("One  Piece/Vol 1.cbz", "alice");
    lib.file("one piece/Vol 2.cbz");
    assert_eq!(
        lib.adopt(),
        Some(FolderAdoption {
            owned: 1,
            unowned: 2
        })
    );
    let rows = lib.db.list_series_folder_owners().unwrap();
    assert_eq!(rows.len(), 1);
    // The smallest spelling that holds a tracked volume.
    assert_eq!(
        (rows[0].folder_name.as_str(), rows[0].owner.as_str()),
        ("Dr  Stone", "alice")
    );
    assert_eq!(lib.owner("KINGDOM HEARTS"), None);
    assert_eq!(lib.owner("ONE PIECE"), None);
}

#[test]
fn an_unreadable_root_waits_for_a_later_start() {
    let lib = Lib::new();
    lib.upload("Solo/Vol 1.cbz", "alice");
    let gone: &Path = &lib.root.join("not-there");
    assert_eq!(adopt_series_folder_owners(&lib.db, gone), None);
    assert!(lib.db.series_folder_adoption_pending().unwrap());
    assert!(lib.adopt().is_some());
    assert_eq!(lib.owner("Solo").as_deref(), Some("alice"));
}

/// A database that tracks no volume (a new one, or one that never had an upload) has
/// nothing to adopt: it is adopted when it is opened.
#[test]
fn a_database_tracking_nothing_is_adopted_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(dir.path().join("mokuro.db"), &DbOptions::default()).unwrap();
    assert!(!db.series_folder_adoption_pending().unwrap());
    assert_eq!(adopt_series_folder_owners(&db, dir.path()), None);
}

/// Only what is there counts (F3, OWN-7): a row whose archive is gone vouches for
/// nothing, and a folder holding what adoption cannot see into gets no owner.
#[test]
fn only_listed_volumes_count_and_hidden_ones_count_against() {
    let lib = Lib::new();
    // Every archive removed outside WebDAV: an empty folder, no owner.
    std::fs::create_dir_all(lib.root.join("Gone")).unwrap();
    lib.db
        .record_volume_upload("Gone/Vol 1.cbz", "alice")
        .unwrap();
    // A stale row of another account does not make a folder mixed.
    lib.upload("Solo/Vol 1.cbz", "alice");
    lib.db
        .record_volume_upload("Solo/Vol 9.cbz", "bob")
        .unwrap();
    // A hidden folder or archive is not seen into: it counts as untracked.
    lib.upload("Hidden/Vol 1.cbz", "alice");
    lib.file("Hidden/.extras/Vol 2.cbz");
    lib.upload("Dotted/Vol 1.cbz", "alice");
    lib.file("Dotted/.Vol 2.cbz");
    // Bunko's own staging file and a hidden non-archive are no volumes.
    lib.upload("Staging/Vol 1.cbz", "alice");
    lib.file("Staging/.Vol 2.cbz.upload-abc.tmp");
    lib.file("Staging/.DS_Store");
    assert_eq!(
        lib.adopt(),
        Some(FolderAdoption {
            owned: 2,
            unowned: 3
        })
    );
    assert_eq!(lib.owner("Solo").as_deref(), Some("alice"));
    assert_eq!(lib.owner("Staging").as_deref(), Some("alice"));
    for folder in ["Gone", "Hidden", "Dotted"] {
        assert_eq!(lib.owner(folder), None, "{folder}");
    }
}

/// A name that is not UTF-8 is not seen into either (OWN-7).
#[cfg(target_os = "linux")]
#[test]
fn a_name_adoption_cannot_read_counts_against() {
    use std::os::unix::ffi::OsStrExt;
    let lib = Lib::new();
    lib.upload("Odd/Vol 1.cbz", "alice");
    let odd = std::ffi::OsStr::from_bytes(b"Vol \xff.cbz");
    std::fs::write(lib.root.join("Odd").join(odd), b"x").unwrap();
    assert_eq!(
        lib.adopt(),
        Some(FolderAdoption {
            owned: 0,
            unowned: 1
        })
    );
    assert_eq!(lib.owner("Odd"), None);
}

/// A first try that cannot adopt is tried again in the same process (F4): until it is
/// done, uploaders' additions wait, so they must not wait for a restart.
#[tokio::test]
async fn adoption_that_cannot_be_done_now_is_tried_again() {
    use std::sync::Arc;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("library");
    let db =
        Arc::new(Database::open_with(dir.path().join("mokuro.db"), &DbOptions::default()).unwrap());
    db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
        .unwrap();
    db.record_volume_upload("Solo/Vol 1.cbz", "alice").unwrap();
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(
        bunko_server::library::adopt_series_folder_owners_until_done(
            db.clone(),
            root.clone(),
            Duration::from_millis(20),
            stop.clone(),
        ),
    );
    // The root cannot be listed yet: still pending, still trying.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!task.is_finished());
    assert!(db.series_folder_adoption_pending().unwrap());
    std::fs::create_dir_all(root.join("Solo")).unwrap();
    std::fs::write(root.join("Solo/Vol 1.cbz"), b"x").unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("adopted once the root can be listed")
        .unwrap();
    assert!(!db.series_folder_adoption_pending().unwrap());
    assert_eq!(
        db.series_folder_owner("Solo").unwrap().as_deref(),
        Some("alice")
    );
    stop.cancel();
}
