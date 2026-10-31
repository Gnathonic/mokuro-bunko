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
        std::fs::create_dir_all(root.join("thumbnails")).unwrap();
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

#[test]
fn every_spelling_of_a_series_counts_together() {
    let lib = Lib::new();
    lib.upload("dr stone/Vol 1.cbz", "alice");
    lib.upload("Dr Stone/Vol 2.cbz", "alice");
    lib.upload("Kingdom/Vol 1.cbz", "alice");
    lib.upload("kingdom/Vol 2.cbz", "bob");
    lib.upload("One/Vol 1.cbz", "alice");
    lib.file("one/Vol 2.cbz");
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
        ("Dr Stone", "alice")
    );
    assert_eq!(lib.owner("KINGDOM"), None);
    assert_eq!(lib.owner("ONE"), None);
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
