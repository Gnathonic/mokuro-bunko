//! index.sqlite: NFC names, merged duplicates, case variants, effective mtimes, moves
//! and sweeps (plan M2 task 2, spec §5.1, §5.2, §5.9).

use bunko_remote::backend::{Backend, ChangeItem, ChangePage, OpCtx, RNode};
use bunko_remote::fake::FakeBackend;
use bunko_remote::index::{ConflictKind, Index, IndexChange, ROOT_EID, RelPath};
use bunko_remote::names::{effective_mtime_ns, fold, nfc};

const G_S: i64 = 1_000_000_000;
const G_MS: i64 = 1_000_000;

fn node(pid: &str, parent: &str, name: &str, is_dir: bool, created_ms: i64) -> RNode {
    RNode {
        pid: pid.into(),
        parent: Some(parent.into()),
        name_raw: name.as_bytes().to_vec(),
        is_dir,
        size: if is_dir { 0 } else { 100 },
        base_mtime_ms: 1_760_000_123_456,
        created_ms,
        version: format!("v-{pid}"),
        cver: format!("c-{pid}"),
        pin: None,
        hash: None,
        trashed: false,
    }
}

fn dir(pid: &str, parent: &str, name: &str, created_ms: i64) -> RNode {
    node(pid, parent, name, true, created_ms)
}

fn file(pid: &str, parent: &str, name: &str, created_ms: i64) -> RNode {
    node(pid, parent, name, false, created_ms)
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s)
}

struct Fx {
    _dir: tempfile::TempDir,
    idx: Index,
}

/// An index with one root `R` (pid "root-1").
fn fx() -> Fx {
    let d = tempfile::tempdir().unwrap();
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    idx.set_roots(&[dir("root-1", "top", "mokuro-reader", 10)])
        .unwrap();
    Fx { _dir: d, idx }
}

impl Fx {
    fn up(&self, n: &RNode) -> Vec<IndexChange> {
        self.up_gen(n, self.idx.generation())
    }

    fn up_gen(&self, n: &RNode, generation: u64) -> Vec<IndexChange> {
        let tx = self.idx.begin().unwrap();
        let ch = self.idx.upsert(&tx, n, generation, G_S).unwrap();
        tx.commit().unwrap();
        ch
    }

    fn rm(&self, pid: &str) -> Vec<IndexChange> {
        let tx = self.idx.begin().unwrap();
        let ch = self.idx.remove(&tx, pid).unwrap();
        tx.commit().unwrap();
        ch
    }

    fn names(&self, path: &str) -> Vec<String> {
        let e = self.idx.lookup(&rel(path)).unwrap();
        self.idx
            .children(e.eid)
            .into_iter()
            .map(|c| c.name)
            .collect()
    }
}

#[test]
fn nfd_name_is_stored_nfc_with_raw_kept() {
    let f = fx();
    let nfd = "\u{30AB}\u{3099}".to_string(); // カ + combining dakuten
    assert_ne!(nfd, "ガ");
    let mut n = dir("d1", "root-1", &nfd, 20);
    n.name_raw = nfd.as_bytes().to_vec();
    let ch = f.up(&n);
    assert_eq!(
        ch,
        [IndexChange::Added {
            rel: rel("ガ"),
            is_dir: true
        }]
    );
    let e = f.idx.lookup(&rel("ガ")).expect("NFC lookup");
    assert_eq!(e.name, "ガ");
    // Lookup is byte-exact: the NFD spelling is not an index name.
    assert!(f.idx.lookup(&RelPath(vec![nfd.clone()])).is_none());
    assert_eq!(f.idx.winner_node(e.eid).unwrap().name_raw, nfd.as_bytes());
    assert_eq!(nfc(nfd.as_bytes()), "ガ");
}

#[test]
fn duplicate_folders_are_one_entry_with_the_union_of_children() {
    let f = fx();
    f.up(&dir("s-new", "root-1", "Series", 300));
    f.up(&dir("s-old", "root-1", "Series", 200));
    let ch = f.up(&file("a", "s-new", "Vol 1.cbz", 400));
    assert_eq!(
        ch,
        [IndexChange::Added {
            rel: rel("Series/Vol 1.cbz"),
            is_dir: false
        }]
    );
    f.up(&file("b", "s-old", "Vol 2.cbz", 401));
    assert_eq!(f.names(""), ["Series"]);
    assert_eq!(f.names("Series"), ["Vol 1.cbz", "Vol 2.cbz"]);

    let e = f.idx.lookup(&rel("Series")).unwrap();
    assert_eq!(e.members, 2);
    assert_eq!(
        e.winner_pid.as_deref(),
        Some("s-old"),
        "oldest created wins"
    );
    assert_eq!(e.created_ms, 200);
    assert_eq!(f.idx.winner_node(e.eid).unwrap().pid, "s-old");
    assert_eq!(f.idx.entry_by_pid("s-new").unwrap().eid, e.eid);

    // Same age: the smallest pid wins.
    f.up(&dir("t-2", "root-1", "Tie", 500));
    f.up(&dir("t-1", "root-1", "Tie", 500));
    let t = f.idx.lookup(&rel("Tie")).unwrap();
    assert_eq!(t.winner_pid.as_deref(), Some("t-1"));

    // Losing one member drops only its children.
    let ch = f.rm("s-old");
    assert_eq!(
        ch,
        [IndexChange::Removed {
            rel: rel("Series/Vol 2.cbz"),
            is_dir: false
        }]
    );
    let e = f.idx.lookup(&rel("Series")).unwrap();
    assert_eq!((e.members, e.winner_pid.as_deref()), (1, Some("s-new")));
    assert_eq!(f.names("Series"), ["Vol 1.cbz"]);
}

#[test]
fn duplicate_files_newest_created_wins_and_the_loser_is_reported() {
    let f = fx();
    let mut old = file("f-old", "root-1", "a.cbz", 100);
    old.size = 1;
    let mut new = file("f-new", "root-1", "a.cbz", 200);
    new.size = 2;
    f.up(&new);
    let ch = f.up(&old);
    assert!(ch.is_empty(), "an older duplicate changes nothing: {ch:?}");
    let e = f.idx.lookup(&rel("a.cbz")).unwrap();
    assert_eq!(e.winner_pid.as_deref(), Some("f-new"));
    assert_eq!(e.size, 2);
    assert_eq!(e.members, 2);
    let c = f.idx.conflicts();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].kind, ConflictKind::DupFile);
    assert_eq!(c[0].pid, "f-old");
    assert_eq!(c[0].rel, Some(rel("a.cbz")));
    assert_eq!(c[0].kind.as_str(), "dup-file");

    // The winner goes: the survivor takes over as a content change.
    let ch = f.rm("f-new");
    assert_eq!(ch, [IndexChange::ContentChanged { rel: rel("a.cbz") }]);
    let e = f.idx.lookup(&rel("a.cbz")).unwrap();
    assert_eq!((e.winner_pid.as_deref(), e.size), (Some("f-old"), 1));
    assert!(f.idx.conflicts().is_empty());
}

#[test]
fn folder_beats_file_of_the_same_name() {
    let f = fx();
    f.up(&file("f", "root-1", "X", 100));
    let ch = f.up(&dir("d", "root-1", "X", 200));
    assert_eq!(
        ch,
        [
            IndexChange::Removed {
                rel: rel("X"),
                is_dir: false
            },
            IndexChange::Added {
                rel: rel("X"),
                is_dir: true
            }
        ]
    );
    assert!(f.idx.lookup(&rel("X")).unwrap().is_dir);
    let c = f.idx.conflicts();
    assert_eq!(
        (c[0].kind, c[0].pid.as_str()),
        (ConflictKind::FileVsDir, "f")
    );
    assert_eq!(c[0].kind.as_str(), "file-vs-dir");
    // The folder goes: the file is visible again.
    f.rm("d");
    assert!(!f.idx.lookup(&rel("X")).unwrap().is_dir);
}

#[test]
fn case_variants_are_distinct_and_variants_lists_oldest_first() {
    let f = fx();
    f.up(&dir("k-upper", "root-1", "Kingdom", 200));
    f.up(&dir("k-lower", "root-1", "kingdom", 100));
    f.up(&dir("k-mixed", "root-1", "KINGDOM", 300));
    f.up(&file("v", "k-upper", "Vol 1.cbz", 400));
    assert_eq!(f.names(""), ["KINGDOM", "Kingdom", "kingdom"]);
    assert!(f.idx.lookup(&rel("kingdom/Vol 1.cbz")).is_none());
    assert!(f.idx.lookup(&rel("Kingdom/Vol 1.cbz")).is_some());
    let v: Vec<String> = f
        .idx
        .variants(ROOT_EID, &fold("Kingdom"))
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(v, ["kingdom", "Kingdom", "KINGDOM"]);
    assert_eq!(fold("ＫiNGDOM"), "ｋingdom");
    assert_eq!(fold("Kingdom"), "kingdom");
}

#[test]
fn effective_mtime_is_deterministic_and_keeps_the_listed_seconds() {
    let base = 1_760_000_123_456; // ms
    let a = effective_mtime_ns(base, G_S, "pid-1", "c1");
    assert_eq!(a, effective_mtime_ns(base, G_S, "pid-1", "c1"));
    assert_ne!(a, effective_mtime_ns(base, G_S, "pid-1", "c2"));
    assert_ne!(a, effective_mtime_ns(base, G_S, "pid-2", "c1"));
    // int(mtime) is the listed seconds; the salt stays below G.
    assert_eq!(a.div_euclid(1_000_000_000), (base / 1000) as i128);
    let ms = effective_mtime_ns(base, G_MS, "pid-1", "c1");
    assert_eq!(ms.div_euclid(1_000_000), base as i128);
    assert_eq!(ms.div_euclid(1_000_000_000), (base / 1000) as i128);
    // The salt is H(pid‖cver) mod G with H the first 8 bytes of sha256(pid 0x00 cver).
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(b"pid-1\0c1");
    let h = u64::from_be_bytes(h[..8].try_into().unwrap());
    assert_eq!(
        a - 1_760_000_000_000_000_000,
        (h % 1_000_000_000) as i128 + 123_000_000_000
    );

    // The index stores exactly that, and a rebuild gives the same value.
    let f = fx();
    let n = file("p", "root-1", "a.cbz", 1);
    f.up(&n);
    let e = f.idx.lookup(&rel("a.cbz")).unwrap();
    assert_eq!(
        e.mtime_ns,
        effective_mtime_ns(n.base_mtime_ms, G_S, &n.pid, &n.cver)
    );
    let g = fx();
    g.up(&n);
    assert_eq!(g.idx.lookup(&rel("a.cbz")).unwrap().mtime_ns, e.mtime_ns);
}

#[test]
fn folder_mtime_bumps_on_child_add_remove_rename() {
    let f = fx();
    let generation = f.idx.bump_generation().unwrap();
    f.up_gen(&dir("s", "root-1", "Series", 1), generation);
    f.up_gen(&file("a", "s", "a.cbz", 2), generation);
    let first = f.idx.lookup(&rel("Series")).unwrap().mtime_ns;
    let d = dir("s", "root-1", "Series", 1);
    assert_eq!(
        first,
        effective_mtime_ns(d.base_mtime_ms, G_S, &d.pid, &d.cver),
        "a folder filled during its own first sync keeps its listed mtime"
    );
    let tx = f.idx.begin().unwrap();
    f.idx.sweep_older_than(&tx, generation, &|_| false).unwrap();
    tx.commit().unwrap();

    let mtime = || f.idx.lookup(&rel("Series")).unwrap().mtime_ns;
    let m0 = mtime();
    f.up(&file("b", "s", "b.cbz", 3));
    let m1 = mtime();
    assert!(m1 >= m0 + 1000, "add bumps");
    f.up(&file("b", "s", "c.cbz", 3));
    let m2 = mtime();
    assert!(m2 >= m1 + 1000, "rename bumps");
    f.rm("b");
    let m3 = mtime();
    assert!(m3 >= m2 + 1000, "remove bumps");
    // A content change of a child is not a folder change.
    let mut a2 = file("a", "s", "a.cbz", 2);
    a2.cver = "c-new".into();
    assert_eq!(
        f.up(&a2),
        [IndexChange::ContentChanged {
            rel: rel("Series/a.cbz")
        }]
    );
    assert_eq!(mtime(), m3);
}

#[test]
fn move_by_pid_emits_moved() {
    let f = fx();
    f.up(&dir("A", "root-1", "A", 1));
    f.up(&dir("B", "root-1", "B", 2));
    f.up(&file("x", "A", "x.cbz", 3));
    let ch = f.up(&file("x", "B", "y.cbz", 3));
    assert_eq!(
        ch,
        [IndexChange::Moved {
            from: rel("A/x.cbz"),
            to: rel("B/y.cbz"),
            is_dir: false
        }]
    );
    // A folder rename carries its subtree.
    let ch = f.up(&dir("B", "root-1", "C", 2));
    assert_eq!(
        ch,
        [IndexChange::Moved {
            from: rel("B"),
            to: rel("C"),
            is_dir: true
        }]
    );
    let x = f.idx.entry_by_pid("x").unwrap();
    assert_eq!(f.idx.path_of(x.eid).unwrap(), rel("C/y.cbz"));
    assert_eq!(f.idx.lookup(&rel("C/y.cbz")).unwrap().eid, x.eid);

    // Moving a folder onto an NFC-equal one merges them.
    f.up(&file("z", "A", "z.cbz", 4));
    let ch = f.up(&dir("A", "root-1", "C", 1));
    assert!(ch.contains(&IndexChange::Removed {
        rel: rel("A"),
        is_dir: true
    }));
    assert!(ch.contains(&IndexChange::Added {
        rel: rel("C/z.cbz"),
        is_dir: false
    }));
    assert_eq!(f.names("C"), ["y.cbz", "z.cbz"]);
    assert_eq!(f.idx.lookup(&rel("C")).unwrap().members, 2);
}

#[test]
fn children_before_parents_are_placed_when_the_parent_arrives() {
    let f = fx();
    assert!(f.up(&file("v", "s", "Vol 1.cbz", 2)).is_empty());
    assert!(f.idx.lookup(&rel("Series")).is_none());
    let ch = f.up(&dir("s", "root-1", "Series", 1));
    assert_eq!(
        ch,
        [
            IndexChange::Added {
                rel: rel("Series"),
                is_dir: true
            },
            IndexChange::Added {
                rel: rel("Series/Vol 1.cbz"),
                is_dir: false
            }
        ]
    );
    // A trashed folder hides its subtree; untrashing brings it back.
    let mut t = dir("s", "root-1", "Series", 1);
    t.trashed = true;
    let ch = f.up(&t);
    assert_eq!(
        ch,
        [
            IndexChange::Removed {
                rel: rel("Series/Vol 1.cbz"),
                is_dir: false
            },
            IndexChange::Removed {
                rel: rel("Series"),
                is_dir: true
            }
        ]
    );
    t.trashed = false;
    assert_eq!(f.up(&t).len(), 2);
    assert!(f.idx.lookup(&rel("Series/Vol 1.cbz")).is_some());
}

#[test]
fn duplicate_deliveries_change_nothing() {
    let f = fx();
    let n = file("a", "root-1", "a.cbz", 1);
    assert_eq!(f.up(&n).len(), 1);
    assert!(f.up(&n).is_empty());
}

#[test]
fn sweep_removes_only_older_generations_and_respects_keep() {
    let f = fx();
    let g1 = f.idx.bump_generation().unwrap();
    for (pid, name) in [("a", "a.cbz"), ("b", "b.cbz"), ("c", "c.cbz")] {
        f.up_gen(&file(pid, "root-1", name, 1), g1);
    }
    let g2 = f.idx.bump_generation().unwrap();
    assert_eq!(g2, g1 + 1);
    f.up_gen(&file("a", "root-1", "a.cbz", 1), g2);
    let tx = f.idx.begin().unwrap();
    let ch = f.idx.sweep_older_than(&tx, g2, &|pid| pid == "c").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        ch,
        [IndexChange::Removed {
            rel: rel("b.cbz"),
            is_dir: false
        }]
    );
    assert_eq!(f.names(""), ["a.cbz", "c.cbz"]);
    // The roots are never swept.
    assert!(f.idx.lookup(&rel("")).is_some());
    assert_eq!(f.idx.roots(), ["root-1"]);
}

#[test]
fn merged_roots_and_nested_roots() {
    let d = tempfile::tempdir().unwrap();
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    let newer = dir("r-new", "top", "mokuro-reader", 50);
    let older = dir("r-old", "top", "mokuro-reader", 40);
    let nested = dir("r-nested", "r-new", "mokuro-reader", 60);
    idx.set_roots(&[newer, older, nested.clone()]).unwrap();
    assert_eq!(idx.roots(), ["r-old", "r-new"], "oldest root first");
    let root = idx.lookup(&rel("")).unwrap();
    assert_eq!(root.eid, ROOT_EID);
    assert_eq!(
        idx.winner_node(ROOT_EID).unwrap().pid,
        "r-old",
        "target = oldest"
    );
    let tx = idx.begin().unwrap();
    idx.upsert(&tx, &file("a", "r-new", "a.cbz", 1), 0, G_S)
        .unwrap();
    idx.upsert(&tx, &file("b", "r-old", "b.cbz", 1), 0, G_S)
        .unwrap();
    // The nested root is an ordinary folder of the tree.
    idx.upsert(&tx, &nested, 0, G_S).unwrap();
    idx.upsert(&tx, &file("c", "r-nested", "c.cbz", 1), 0, G_S)
        .unwrap();
    tx.commit().unwrap();
    let names: Vec<_> = idx.children(ROOT_EID).into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["a.cbz", "b.cbz", "mokuro-reader"]);
    assert!(idx.lookup(&rel("mokuro-reader/c.cbz")).is_some());
    let c = idx.conflicts();
    assert_eq!(c.len(), 1);
    assert_eq!(
        (c[0].kind, c[0].pid.as_str()),
        (ConflictKind::NestedRoot, "r-nested")
    );
    assert_eq!(c[0].rel, Some(rel("mokuro-reader")));
    drop(idx);

    // Generation, meta and roots survive a reopen.
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    assert_eq!(idx.roots(), ["r-old", "r-new"]);
    idx.meta_set("cursor", "42").unwrap();
    let g = idx.bump_generation().unwrap();
    drop(idx);
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    assert_eq!(idx.meta_get("cursor").as_deref(), Some("42"));
    assert_eq!(idx.generation(), g);
    assert!(idx.lookup(&rel("mokuro-reader/c.cbz")).is_some());
}

/// Apply every pending change page of `b` to `idx`, one transaction per page.
async fn apply_feed(b: &FakeBackend, idx: &Index, mut c: bunko_remote::Cursor) -> Vec<IndexChange> {
    let g = b.caps().mtime_granularity_ns;
    let mut all = Vec::new();
    loop {
        let ChangePage::Page { items, next, more } = b.changes(&c).await.unwrap() else {
            panic!("cursor lost");
        };
        let tx = idx.begin().unwrap();
        for item in &items {
            all.extend(match item {
                ChangeItem::Upsert(n) => idx.upsert(&tx, n, idx.generation(), g).unwrap(),
                ChangeItem::Removed { pid } => idx.remove(&tx, pid).unwrap(),
            });
        }
        idx.meta_set("cursor", &next.0).unwrap();
        tx.commit().unwrap();
        c = next;
        if !more {
            return all;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn fake_feed_drives_the_index_on_every_profile() {
    for b in [
        FakeBackend::drive_like(),
        FakeBackend::webdav_cas(),
        FakeBackend::webdav_plain(),
        FakeBackend::onedrive_like(),
        FakeBackend::mega_like(),
    ] {
        let label = b.account().await.unwrap().label;
        b.reader_put("mokuro-reader/Series/Vol 1.cbz", b"zip");
        b.reader_put("mokuro-reader/Series/Vol 1.mokuro", b"{}");
        let d = tempfile::tempdir().unwrap();
        let idx = Index::open(&d.path().join("index.sqlite")).unwrap();

        // Full sync: cursor first, then enumerate, then sweep.
        idx.set_roots(&b.locate_roots("mokuro-reader").await.unwrap())
            .unwrap();
        let cursor = b.start_cursor().await.unwrap();
        let generation = idx.bump_generation().unwrap();
        let mut nodes = Vec::new();
        b.list_all(&idx.roots(), &mut |n| nodes.push(n))
            .await
            .unwrap();
        let tx = idx.begin().unwrap();
        let mut added = Vec::new();
        for n in &nodes {
            added.extend(
                idx.upsert(&tx, n, generation, b.caps().mtime_granularity_ns)
                    .unwrap(),
            );
        }
        idx.sweep_older_than(&tx, generation, &|_| false).unwrap();
        tx.commit().unwrap();
        assert_eq!(added.len(), 3, "{label}: {added:?}");

        b.reader_rename("mokuro-reader/Series", "mokuro-reader/Renamed");
        b.reader_put("mokuro-reader/Renamed/Vol 2.cbz", b"zip2");
        b.reader_delete("mokuro-reader/Renamed/Vol 1.mokuro");
        b.reader_put("mokuro-reader/Renamed/Vol 1.cbz", b"zip, edited");
        let changes = apply_feed(&b, &idx, cursor).await;

        assert!(idx.lookup(&rel("Series")).is_none(), "{label}");
        let names: Vec<_> = idx
            .children(idx.lookup(&rel("Renamed")).unwrap().eid)
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["Vol 1.cbz", "Vol 2.cbz"], "{label}: {changes:?}");
        let v1 = idx.lookup(&rel("Renamed/Vol 1.cbz")).unwrap();
        assert_eq!(v1.size, 11, "{label}");
        let w = idx.winner_node(v1.eid).unwrap();
        assert_eq!(b.content(&w.pid).unwrap(), b"zip, edited", "{label}");
        if b.caps().path_ids {
            assert!(changes.contains(&IndexChange::Removed {
                rel: rel("Series"),
                is_dir: true
            }));
        } else {
            assert!(changes.contains(&IndexChange::Moved {
                from: rel("Series"),
                to: rel("Renamed"),
                is_dir: true
            }));
        }
    }

    // Drive: a duplicate folder from a racing tab merges into the same entry.
    let b = FakeBackend::drive_like();
    b.reader_put("mokuro-reader/Series/Vol 1.cbz", b"zip");
    let d = tempfile::tempdir().unwrap();
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    let roots = b.locate_roots("mokuro-reader").await.unwrap();
    idx.set_roots(&roots).unwrap();
    let cursor = b.start_cursor().await.unwrap();
    let mut nodes = Vec::new();
    b.list_all(&idx.roots(), &mut |n| nodes.push(n))
        .await
        .unwrap();
    let tx = idx.begin().unwrap();
    for n in &nodes {
        idx.upsert(&tx, n, 0, b.caps().mtime_granularity_ns)
            .unwrap();
    }
    tx.commit().unwrap();
    let dup = b
        .mkdir(&roots[0].pid, "Series", &OpCtx::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("v2");
    std::fs::write(&src, b"zip2").unwrap();
    use sha2::Digest;
    b.upload(
        bunko_remote::UploadTarget::New {
            parent: &dup.pid,
            name: "Vol 2.cbz",
        },
        bunko_remote::UploadSource::File {
            path: &src,
            size: 4,
            sha256: sha2::Sha256::digest(b"zip2").into(),
        },
        &mut OpCtx::default(),
    )
    .await
    .unwrap();
    let changes = apply_feed(&b, &idx, cursor).await;
    assert_eq!(
        changes,
        [IndexChange::Added {
            rel: rel("Series/Vol 2.cbz"),
            is_dir: false
        }]
    );
    let s = idx.lookup(&rel("Series")).unwrap();
    assert_eq!(s.members, 2);
    assert_ne!(
        s.winner_pid.as_deref(),
        Some(dup.pid.as_str()),
        "the older folder wins"
    );
}
