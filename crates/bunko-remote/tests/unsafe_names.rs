//! Provider names are untrusted: a name that cannot be one path segment never becomes
//! an index entry or a `RelPath` segment (no `..` escape, no `a/b` aliasing).

use bunko_remote::backend::{Backend, OpCtx, RNode, UploadSource, UploadTarget};
use bunko_remote::fake::FakeBackend;
use bunko_remote::index::{ConflictKind, Index, IndexChange, ROOT_EID, RelPath};
use bunko_remote::names::is_safe_segment;
use sha2::{Digest, Sha256};

const BAD: [&str; 7] = ["..", ".", "", "a/b", "a\\b", "x\0y", "\u{7}"];

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("a safe test path")
}

#[test]
fn safe_segment_rule() {
    for bad in BAD.iter().chain(&[
        "\u{7f}",
        "tab\there",
        "nl\n",
        "/",
        "\\",
        "a:b",
        "C:x",
        "C:",
        "x:",
    ]) {
        assert!(!is_safe_segment(bad), "{bad:?}");
    }
    for good in [
        "Vol 1.cbz",
        "...",
        "..a",
        ".hidden",
        "ガ",
        "\u{fffd}",
        "a.b c",
    ] {
        assert!(is_safe_segment(good), "{good:?}");
    }
}

#[test]
fn rel_path_constructors_refuse_unsafe_segments() {
    for s in [
        "../x", "./x", "a/../b", "a/./b", "..", "a\\b", "x\0y", "a/\u{7}",
    ] {
        assert_eq!(RelPath::parse(s), None, "{s:?}");
    }
    assert_eq!(rel("a//b/").segments(), ["a", "b"]);
    assert!(rel("").is_root());
    assert_eq!(RelPath::from_segments(["a/b"]), None);
    assert_eq!(RelPath::from_segments(["ok", ".."]), None);
    assert_eq!(RelPath::from_segments([""]), None);
    let mut p = RelPath::root();
    assert_eq!(p.push_checked(".."), Err("..".to_string()));
    assert!(p.push_checked("ok").is_ok());
    assert_eq!(p.join("a/b"), None);
    assert_eq!(p.join("c").unwrap().to_string(), "ok/c");
}

async fn put(b: &FakeBackend, parent: &str, name: &str, bytes: &[u8]) -> RNode {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::write(&src, bytes).unwrap();
    b.upload(
        UploadTarget::New { parent, name },
        UploadSource::File {
            path: &src,
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        },
        &mut OpCtx::default(),
    )
    .await
    .unwrap()
}

fn names(idx: &Index, eid: i64) -> Vec<String> {
    idx.children(eid).into_iter().map(|e| e.name).collect()
}

#[tokio::test(start_paused = true)]
async fn unsafe_names_are_not_indexed_on_drive_and_mega() {
    for b in [FakeBackend::drive_like(), FakeBackend::mega_like()] {
        let label = b.account().await.unwrap().label;
        let root = b.reader_mkdir("mokuro-reader");
        let op = OpCtx::default();
        let mut bad_dirs = Vec::new();
        let mut inner = Vec::new();
        for name in BAD {
            let d = b.mkdir(&root.pid, name, &op).await.unwrap();
            inner.push(put(&b, &d.pid, "inner.cbz", b"x").await.pid);
            bad_dirs.push(d.pid);
        }
        let good = b.mkdir(&root.pid, "Good", &op).await.unwrap();
        put(&b, &good.pid, "ok.cbz", b"ok").await;
        let bad_file = put(&b, &good.pid, "..", b"evil").await;

        let d = tempfile::tempdir().unwrap();
        let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
        idx.set_roots(&b.locate_roots("mokuro-reader").await.unwrap())
            .unwrap();
        let mut nodes = Vec::new();
        b.list_all(&idx.roots(), &mut |n| nodes.push(n))
            .await
            .unwrap();
        // Children before parents too, so placement is retried on every path.
        nodes.reverse();
        let tx = idx.begin().unwrap();
        let mut changes = Vec::new();
        for n in &nodes {
            changes.extend(
                idx.upsert(&tx, n, 0, b.caps().mtime_granularity_ns)
                    .unwrap(),
            );
        }
        tx.commit().unwrap();

        // Only the normal sibling and its normal child are visible.
        assert_eq!(names(&idx, ROOT_EID), ["Good"], "{label}");
        let g = idx.lookup(&rel("Good")).unwrap();
        assert_eq!(names(&idx, g.eid), ["ok.cbz"], "{label}");
        let mut added: Vec<String> = changes
            .iter()
            .map(|c| match c {
                IndexChange::Added { rel, .. } => rel.to_string(),
                other => panic!("{label}: unexpected {other:?}"),
            })
            .collect();
        added.sort();
        assert_eq!(added, ["Good", "Good/ok.cbz"], "{label}");

        // The subtrees of invalid folders are unreachable.
        for pid in inner.iter().chain(&bad_dirs).chain([&bad_file.pid]) {
            assert_eq!(idx.entry_by_pid(pid), None, "{label}: {pid}");
        }

        // Each invalid name is reported once, under its parent's path.
        let mut c: Vec<_> = idx
            .conflicts()
            .into_iter()
            .filter(|c| c.kind == ConflictKind::InvalidName)
            .collect();
        assert_eq!(c.len(), BAD.len() + 1, "{label}: {c:?}");
        c.sort_by(|a, b| a.pid.cmp(&b.pid));
        for name in BAD {
            let pid = &bad_dirs[BAD.iter().position(|n| *n == name).unwrap()];
            let conflict = c.iter().find(|c| &c.pid == pid).expect("reported");
            assert_eq!(conflict.rel, Some(RelPath::root()), "{label}");
            assert_eq!(
                conflict.detail.as_deref(),
                Some(format!("{name:?}").as_str())
            );
        }
        let f = c.iter().find(|c| c.pid == bad_file.pid).unwrap();
        assert_eq!(f.rel, Some(rel("Good")));
        assert_eq!(ConflictKind::InvalidName.as_str(), "invalid-name");
        assert!(
            !c.iter().any(|c| inner.contains(&c.pid)),
            "only the top of a hidden subtree is reported"
        );
    }
}

fn dir(pid: &str, parent: &str, name: &str) -> RNode {
    RNode {
        pid: pid.into(),
        parent: Some(parent.into()),
        name_raw: name.as_bytes().to_vec(),
        is_dir: true,
        size: 0,
        base_mtime_ms: 1_760_000_000_000,
        created_ms: 1,
        version: "v".into(),
        cver: format!("c-{pid}"),
        pin: None,
        hash: None,
        trashed: false,
    }
}

#[test]
fn renaming_to_an_unsafe_name_hides_the_subtree_and_back_restores_it() {
    let d = tempfile::tempdir().unwrap();
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    idx.set_roots(&[dir("r", "top", "mokuro-reader")]).unwrap();
    let up = |n: &RNode| {
        let tx = idx.begin().unwrap();
        let ch = idx.upsert(&tx, n, 0, 1_000_000_000).unwrap();
        tx.commit().unwrap();
        ch
    };
    up(&dir("s", "r", "Series"));
    up(&dir("v", "s", "Vol"));
    let ch = up(&dir("s", "r", ".."));
    assert_eq!(
        ch,
        [
            IndexChange::Removed {
                rel: rel("Series/Vol"),
                is_dir: true
            },
            IndexChange::Removed {
                rel: rel("Series"),
                is_dir: true
            }
        ]
    );
    assert!(idx.children(ROOT_EID).is_empty());
    assert!(idx.lookup(&rel("Vol")).is_none());
    let c = idx.conflicts();
    assert_eq!((c.len(), c[0].kind), (1, ConflictKind::InvalidName));
    let ch = up(&dir("s", "r", "Series"));
    assert_eq!(ch.len(), 2);
    assert!(idx.lookup(&rel("Series/Vol")).is_some());
    assert!(idx.conflicts().is_empty());
}

#[test]
fn root_candidates_with_unsafe_names_are_ignored() {
    let d = tempfile::tempdir().unwrap();
    let idx = Index::open(&d.path().join("index.sqlite")).unwrap();
    let mut evil = dir("evil", "top", "..");
    evil.created_ms = 0; // older: it would otherwise be the target root
    idx.set_roots(&[evil, dir("r", "top", "mokuro-reader")])
        .unwrap();
    assert_eq!(idx.roots(), ["r"]);
    assert_eq!(idx.winner_node(ROOT_EID).unwrap().pid, "r");
}
