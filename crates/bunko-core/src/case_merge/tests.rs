//! The planner's rules over an in-memory [`FakeHost`], one golden plan per rule.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

/// An in-memory library: node ids are library paths (`""` the root), names NFC
/// unless a test lists another form on purpose, content is the node's facts.
#[derive(Default)]
struct FakeHost {
    kids: HashMap<String, Vec<Node<Vec<u8>>>>,
    /// Ids whose listing fails (a read the store cannot make now).
    failing: HashSet<String>,
    /// List children in reverse order (a provider's unspecified listing order).
    reversed: bool,
}

impl FakeHost {
    /// `paths` in creation order (earlier is older); a trailing `/` is a folder.
    fn new(paths: &[(&str, &str)]) -> FakeHost {
        let mut h = FakeHost::default();
        h.kids.insert(String::new(), Vec::new());
        for (i, (p, content)) in paths.iter().enumerate() {
            let is_dir = p.ends_with('/');
            let id = p.trim_end_matches('/').to_string();
            let (parent, name) = id.rsplit_once('/').unwrap_or(("", id.as_str()));
            let node = Node {
                id: id.clone(),
                name: name.to_string(),
                is_dir,
                created_ms: i as i64 + 1,
                ver: format!("v-{content}"),
                size: content.len() as u64,
                x: content.as_bytes().to_vec(),
            };
            h.kids.entry(parent.to_string()).or_default().push(node);
            if is_dir {
                h.kids.entry(id).or_default();
            }
        }
        h
    }
}

impl Host for FakeHost {
    type X = Vec<u8>;

    fn list(&self, id: &str) -> Result<Vec<Node<Vec<u8>>>, Defer> {
        if self.failing.contains(id) {
            return Err(defer(format!("{id}: no listing now")));
        }
        let mut v = self.kids.get(id).cloned().unwrap_or_default();
        if self.reversed {
            v.reverse();
        }
        Ok(v)
    }

    fn identical(&self, a: &Node<Vec<u8>>, b: &Node<Vec<u8>>) -> Result<bool, Defer> {
        Ok(!a.is_dir && !b.is_dir && a.x == b.x)
    }

    fn path_of(&self, _n: &Node<Vec<u8>>, rel: &RelPath) -> PathBuf {
        Path::new("/lib").join(rel.to_string())
    }

    fn replace_bytes(&self, n: &Node<Vec<u8>>) -> Result<Option<Vec<u8>>, Defer> {
        Ok(Some(n.x.clone()))
    }
}

struct Policy {
    choice: MergeChoice,
    asked: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl Policy {
    fn new(choice: MergeChoice) -> Policy {
        Policy {
            choice,
            asked: Mutex::default(),
        }
    }
}

impl CaseMergePolicy for Policy {
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice {
        self.asked
            .lock()
            .unwrap()
            .push((a.to_path_buf(), b.to_path_buf()));
        self.choice
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

/// A folder set at the root: `winner` and `losers` are folder ids (one member each).
fn folders(winner: &str, losers: &[&str]) -> SetSpec {
    SetSpec {
        parent: RelPath::root(),
        stem: false,
        winner: (rel(winner), vec![winner.to_string()]),
        losers: losers
            .iter()
            .map(|l| (rel(l), vec![l.to_string()]))
            .collect(),
        ..SetSpec::default()
    }
}

/// A same-folder stem set in the folder `parent` (one member, its own target).
fn stems(parent: &str, winner: &str, group: &str) -> SetSpec {
    let p = RelPath::parse(parent).unwrap();
    SetSpec {
        winner: (p.join(winner).unwrap(), Vec::new()),
        parent: p,
        stem: true,
        group: group.to_string(),
        parent_ids: vec![parent.to_string()],
        target: Some(parent.to_string()),
        ..SetSpec::default()
    }
}

/// The plan as lines, in the order it runs, then its blocks.
fn lines(plan: &Plan) -> Vec<String> {
    let mut out: Vec<String> = plan
        .ordered()
        .map(|p| {
            let src = p.src.as_ref().map(ToString::to_string).unwrap_or_default();
            match p.kind {
                StepKind::Move | StepKind::Replace => {
                    format!("{:?} {src} -> {}", p.kind, p.dest)
                }
                StepKind::Submit => format!(
                    "Submit {} -> {}",
                    p.dest,
                    p.series
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                ),
                StepKind::Trash | StepKind::Finish => format!("{:?} {}", p.kind, p.dest),
            }
        })
        .collect();
    for b in &plan.blocks {
        let s = |v: &[RelPath]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        out.push(format!(
            "Block {:?} | {:?}: {}",
            s(&b.this),
            s(&b.that),
            b.detail
        ));
    }
    out
}

fn plan(h: &FakeHost, choice: MergeChoice, spec: &SetSpec) -> Plan {
    plan_set(h, &Policy::new(choice), spec).expect("a plan")
}

#[test]
fn loser_only_files_and_folders_move_in_and_the_loser_finishes() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("kingdom/", ""),
        ("kingdom/Vol 2.cbz", "b"),
        ("kingdom/Vol 2.mokuro", "ocr"),
        ("kingdom/Extras/", ""),
        ("kingdom/Extras/x.cbz", "x"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Move kingdom/Extras -> Kingdom/Extras",
            "Move kingdom/Vol 2.cbz -> Kingdom/Vol 2.cbz",
            "Move kingdom/Vol 2.mokuro -> Kingdom/Vol 2.mokuro",
            "Finish kingdom",
        ]
    );
    assert!(p.work());
    let moved = &p.steps[0];
    assert_eq!(moved.parent_id.as_deref(), Some("Kingdom"));
    assert!(moved.is_dir && moved.hooks);
    // What moved in is taken in the target.
    let taken = &p.taken["Kingdom"];
    for n in ["vol 1.cbz", "extras", "vol 2.cbz", "vol 2.mokuro"] {
        assert!(taken.contains(n), "{n}");
    }
}

#[test]
fn folders_of_one_fold_merge_recursively_deepest_finish_first() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Extras/", ""),
        ("Kingdom/Extras/a.cbz", "a"),
        ("kingdom/", ""),
        ("kingdom/EXTRAS/", ""),
        ("kingdom/EXTRAS/b.cbz", "b"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Move kingdom/EXTRAS/b.cbz -> Kingdom/Extras/b.cbz",
            "Finish kingdom/EXTRAS",
            "Finish kingdom",
        ]
    );
    // A finish names where queued writes for the loser go instead.
    assert_eq!(p.finishes[0].src, Some(rel("Kingdom/Extras")));
}

#[test]
fn identical_files_are_trashed_and_an_empty_loser_is_work() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.mokuro", "ocr"),
        ("Kingdom/Vol 1.webp", "c1"),
        ("kingdom/", ""),
        ("kingdom/vol 1.cbz", "a"),
        ("kingdom/vol 1.mokuro", "ocr"),
        // A differing cover is superseded, never a block.
        ("kingdom/vol 1.webp", "c2"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Trash kingdom/vol 1.cbz",
            "Trash kingdom/vol 1.mokuro",
            "Trash kingdom/vol 1.webp",
            "Finish kingdom",
        ]
    );
    let empty = FakeHost::new(&[("Kingdom/", ""), ("kingdom/", "")]);
    let p = plan(
        &empty,
        MergeChoice::Block,
        &folders("Kingdom", &["kingdom"]),
    );
    assert!(p.empty_loser && p.work());
    assert_eq!(lines(&p), ["Finish kingdom"]);
}

#[test]
fn differing_archives_block_their_volume_group_only() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "DIFFERENT"),
        ("kingdom/Vol 1.mokuro", "ocr"),
        ("kingdom/Vol 2.cbz", "b"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Move kingdom/Vol 2.cbz -> Kingdom/Vol 2.cbz",
            "Finish kingdom",
            "Block [\"kingdom/Vol 1.cbz\", \"kingdom/Vol 1.mokuro\"] | [\"Kingdom/Vol 1.cbz\"]: \
             kingdom/Vol 1.cbz and Kingdom/Vol 1.cbz are different archives",
        ]
    );
}

fn mains() -> FakeHost {
    FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.mokuro", "ocr-w"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("kingdom/Vol 1.mokuro", "ocr-l"),
    ])
}

#[test]
fn differing_mains_beside_identical_archives_ask_the_policy() {
    let h = mains();
    let spec = folders("Kingdom", &["kingdom"]);
    let policy = Policy::new(MergeChoice::A);
    let a = plan_set(&h, &policy, &spec).unwrap();
    assert_eq!(
        lines(&a),
        [
            "Trash kingdom/Vol 1.cbz",
            "Trash kingdom/Vol 1.mokuro",
            "Finish kingdom"
        ]
    );
    assert_eq!(
        *policy.asked.lock().unwrap(),
        [(
            PathBuf::from("/lib/Kingdom/Vol 1.mokuro"),
            PathBuf::from("/lib/kingdom/Vol 1.mokuro")
        )]
    );
    let b = plan(&h, MergeChoice::B, &spec);
    assert_eq!(
        lines(&b),
        [
            "Replace kingdom/Vol 1.mokuro -> Kingdom/Vol 1.mokuro",
            "Trash kingdom/Vol 1.cbz",
            "Trash kingdom/Vol 1.mokuro",
            "Finish kingdom"
        ]
    );
    let r = &b.steps[0];
    assert_eq!(
        r.id, "Kingdom/Vol 1.mokuro",
        "the winner's node is replaced"
    );
    assert_eq!(
        r.ver.as_deref(),
        Some("v-ocr-w"),
        "based on the judged version"
    );
    assert_eq!(r.bytes.as_deref(), Some(b"ocr-l".as_slice()));
    let block = plan(&h, MergeChoice::Block, &spec);
    assert_eq!(block.blocks.len(), 1);
    assert!(
        block.blocks[0].detail.contains("neither (or both)"),
        "{:?}",
        block.blocks
    );
    assert!(block.steps.is_empty() && block.l_trash.is_empty());
}

#[test]
fn a_gz_main_or_differing_archives_never_reach_the_policy() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.mokuro.gz", "ocr-w"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("kingdom/Vol 1.mokuro.gz", "ocr-l"),
    ]);
    let policy = Policy::new(MergeChoice::B);
    let p = plan_set(&h, &policy, &folders("Kingdom", &["kingdom"])).unwrap();
    assert!(policy.asked.lock().unwrap().is_empty());
    assert_eq!(p.blocks.len(), 1);
    assert!(
        p.blocks[0]
            .detail
            .ends_with("hold different OCR for this volume")
    );
    // The archive's own trash is dropped with its blocked group.
    assert_eq!(lines(&p)[..1], ["Finish kingdom"]);
}

#[test]
fn an_unstamped_layer_moves_in_under_the_next_id_free_by_the_taken_fold() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.edit.mokuro", "e-w"),
        // `edit-2` is taken in another spelling: a zero-width space, a dotless ı,
        // and (for curated) a decomposed stem fold onto it.
        ("Kingdom/Vol 1.edit\u{200B}-2.mokuro", "z"),
        ("Kingdom/Vol 1.ed\u{131}t-3.mokuro", "i"),
        ("Kingdom/Vol 1.curated.mokuro", "c-w"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("kingdom/Vol 1.edit.mokuro", "e-l"),
        ("kingdom/Vol 1.curated.mokuro", "c-l"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Move kingdom/Vol 1.edit.mokuro -> Kingdom/Vol 1.edit-4.mokuro",
            "Move kingdom/Vol 1.curated.mokuro -> Kingdom/Vol 1.curated-2.mokuro",
            "Trash kingdom/Vol 1.cbz",
            "Finish kingdom",
        ]
    );
}

#[test]
fn free_layer_counts_from_the_stripped_id_and_skips_every_taken_fold() {
    let mut p = Plan::default();
    p.taken.insert(
        "t".into(),
        ["caf\u{E9}.edit.mokuro", "caf\u{E9}.edit-2.mokuro"]
            .iter()
            .map(|n| taken_key(n))
            .collect(),
    );
    // A decomposed stem folds like the composed names taken.
    assert_eq!(
        free_layer(&mut p, "t", "Cafe\u{301}", "edit-2", ".mokuro"),
        "Cafe\u{301}.edit-3.mokuro"
    );
    // A free id is taken from then on.
    assert_eq!(
        free_layer(&mut p, "t", "Café", "edit", ".mokuro"),
        "Café.edit-4.mokuro"
    );
    // A counted id whose base is free moves in as the base.
    assert_eq!(
        free_layer(&mut p, "t", "Café", "curated-3", ".mokuro"),
        "Café.curated.mokuro"
    );
}

#[test]
fn a_digit_only_id_is_a_layer_beside_its_archive_and_a_stamped_layer_blocks() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.2.mokuro", "w"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("kingdom/Vol 1.2.mokuro", "l"),
    ]);
    let p = plan(&h, MergeChoice::B, &folders("Kingdom", &["kingdom"]));
    assert_eq!(p.blocks.len(), 1);
    assert_eq!(
        p.blocks[0].detail,
        "kingdom/Vol 1.2.mokuro and Kingdom/Vol 1.2.mokuro differ"
    );
    let stems = archive_stems(["Vol 1.cbz"].into_iter());
    assert_eq!(
        split_name("Vol 1.2.mokuro", &stems).2,
        Part::Layer("2".into())
    );
    // Beside a decimal volume's archive it is that volume's main.
    let stems = archive_stems(["Vol 1.cbz", "Vol 1.2.cbz"].into_iter());
    assert_eq!(split_name("Vol 1.2.mokuro", &stems).2, Part::Main);
}

#[test]
fn a_series_json_is_submitted_into_the_winner_only_at_series_depth() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/series.json", "{\"w\":1}"),
        ("Kingdom/Extras/", ""),
        ("Kingdom/Extras/series.json", "{\"w\":1}"),
        ("kingdom/", ""),
        ("kingdom/SERIES.json", "{\"l\":1}"),
        ("kingdom/Extras/", ""),
        ("kingdom/Extras/series.json", "{\"l\":1}"),
    ]);
    let p = plan(&h, MergeChoice::Block, &folders("Kingdom", &["kingdom"]));
    assert_eq!(
        lines(&p),
        [
            "Submit kingdom/SERIES.json -> Kingdom/series.json",
            "Finish kingdom/Extras",
            "Finish kingdom",
            "Block [\"kingdom/Extras/series.json\"] | [\"Kingdom/Extras/series.json\"]: \
             kingdom/Extras/series.json and Kingdom/Extras/series.json differ",
        ]
    );
    let s = &p.submits[0];
    assert_eq!(s.ver.as_deref(), Some("v-{\"l\":1}"));
    assert!(s.hooks);
}

#[test]
fn a_file_and_a_folder_of_one_fold_block() {
    let h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/extras", "file"),
        ("Kingdom/Notes/", ""),
        ("kingdom/", ""),
        ("kingdom/Extras/", ""),
        ("kingdom/notes", "file"),
    ]);
    let mut spec = folders("Kingdom", &["kingdom"]);
    spec.files_vs = vec![rel("KINGDOM")];
    let p = plan(&h, MergeChoice::Block, &spec);
    let detail = "a file and a folder whose names differ only in case";
    assert_eq!(
        lines(&p),
        [
            "Finish kingdom".to_string(),
            format!("Block [\"KINGDOM\"] | [\"Kingdom\"]: {detail}"),
            format!("Block [\"kingdom/Extras\"] | [\"Kingdom/extras\"]: {detail}"),
            // The winner's side is named with the loser's spelling (today's rule,
            // pinned as it is: the extraction changes nothing).
            format!("Block [\"kingdom/notes\"] | [\"Kingdom/notes\"]: {detail}"),
        ]
    );
}

#[test]
fn a_stem_set_renames_into_the_archive_anchored_spelling() {
    // `order_stem_members`: archives first, then the oldest, then the id; one
    // member per exact stem.
    let mut v = vec![
        ("VOL.mokuro", 1, "a"),
        ("Vol.mokuro", 2, "b"),
        ("Vol.cbz", 3, "c"),
    ];
    let st = archive_stems(v.iter().map(|e| e.0));
    order_stem_members(&mut v, &st, |e| e.0, |e| (e.1, e.2));
    assert_eq!(
        v.iter().map(|e| e.0).collect::<Vec<_>>(),
        ["Vol.cbz", "VOL.mokuro"]
    );

    let h = FakeHost::new(&[
        ("K/", ""),
        ("K/VOL.mokuro", "ocr"),
        ("K/Vol.cbz", "a"),
        ("K/Other.cbz", "o"),
    ]);
    let (_, group) = file_key("Vol.cbz", &st);
    let p = plan(&h, MergeChoice::Block, &stems("K", "Vol.cbz", &group));
    assert_eq!(lines(&p), ["Move K/VOL.mokuro -> K/Vol.mokuro"]);
    assert_eq!(p.steps[0].parent_id.as_deref(), Some("K"));
    // The whole folder is taken in the target, the new name too.
    assert!(p.taken["K"].contains("other.cbz") && p.taken["K"].contains("vol.mokuro"));
    // No node to rename in: deferred, nothing listed.
    let mut none = stems("K", "Vol.cbz", &group);
    none.target = None;
    assert_eq!(
        plan_set(&h, &Policy::new(MergeChoice::Block), &none).unwrap_err(),
        "the folder has no node"
    );
}

#[test]
fn every_member_of_both_spellings_is_listed_and_a_failed_read_defers() {
    let mut h = FakeHost::new(&[
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("KINGDOM/", ""),
        ("KINGDOM/Vol 2.cbz", "b"),
    ]);
    let spec = folders("Kingdom", &["KINGDOM", "kingdom"]);
    assert_eq!(
        lines(&plan(&h, MergeChoice::Block, &spec)),
        [
            "Move KINGDOM/Vol 2.cbz -> Kingdom/Vol 2.cbz",
            "Trash kingdom/Vol 1.cbz",
            "Finish KINGDOM",
            "Finish kingdom",
        ]
    );
    h.failing.insert("kingdom".into());
    assert_eq!(
        plan_set(&h, &Policy::new(MergeChoice::Block), &spec).unwrap_err(),
        "kingdom: no listing now"
    );
    let mut none = folders("Kingdom", &["kingdom"]);
    none.winner.1.clear();
    assert_eq!(
        plan_set(&h, &Policy::new(MergeChoice::Block), &none).unwrap_err(),
        "the winning folder has no node"
    );
}

#[test]
fn the_fingerprint_ignores_listing_order_and_sees_every_version() {
    let files = [
        ("Kingdom/", ""),
        ("Kingdom/Vol 1.cbz", "a"),
        ("Kingdom/Vol 1.edit.mokuro", "e"),
        ("kingdom/", ""),
        ("kingdom/Vol 1.cbz", "a"),
        ("kingdom/Vol 2.cbz", "b"),
        ("kingdom/Vol 3.cbz", "c"),
        ("kingdom/Extras/", ""),
    ];
    let spec = folders("Kingdom", &["kingdom"]);
    let mut h = FakeHost::new(&files);
    let forward = plan(&h, MergeChoice::Block, &spec);
    h.reversed = true;
    let backward = plan(&h, MergeChoice::Block, &spec);
    assert_ne!(
        lines(&forward),
        lines(&backward),
        "the listings differ in order"
    );
    assert_eq!(forward.fingerprint(), backward.fingerprint());
    let mut changed = files;
    changed[5] = ("kingdom/Vol 2.cbz", "b2");
    let other = plan(&FakeHost::new(&changed), MergeChoice::Block, &spec);
    assert_ne!(forward.fingerprint(), other.fingerprint());
}

#[test]
fn scopes_keep_their_stored_form() {
    let s = Scope {
        dir: vec!["kingdom".into()],
        key: "vol 1".into(),
        stem: true,
        shallow: false,
    };
    assert_eq!(
        s.json(),
        r#"{"dir":["kingdom"],"key":"vol 1","stem":true,"shallow":false}"#
    );
    assert!(s.covers(&["kingdom".into(), "vol 1.mokuro".into()]));
    assert!(!s.covers(&["kingdom".into(), "vol 10.cbz".into()]));
    let d = Scope::dup(&rel("Kingdom/Extras"));
    assert_eq!(
        (d.dir.as_slice(), d.key.as_str()),
        (["kingdom".to_string()].as_slice(), "extras")
    );
    assert!(d.covers(&["kingdom".into(), "extras".into(), "a.cbz".into()]));
    assert!(!d.covers(&[
        "kingdom".into(),
        "extras".into(),
        "a".into(),
        "b.cbz".into()
    ]));
    let root = Scope::dup(&RelPath::root());
    assert!(root.covers(&["kingdom".into()]));
    assert!(!root.covers(&[".mokuro-bunko.json".into()]));
}

#[test]
fn the_stored_step_fields_keep_their_names() {
    let ex = Extra {
        step: StepKind::Replace,
        vg: "k/vol 1".into(),
        is_dir: false,
        cver: Some("7".into()),
        hooks: true,
        series: None,
        expect_dup: false,
    };
    assert_eq!(
        serde_json::to_string(&ex).unwrap(),
        r#"{"step":"replace","vg":"k/vol 1","is_dir":false,"cver":"7","hooks":true,"series":null,"expect_dup":false}"#
    );
    let old: Extra = serde_json::from_str(r#"{"step":"finish","vg":"k/"}"#).unwrap();
    assert_eq!(
        (old.step, old.hooks, old.cver),
        (StepKind::Finish, false, None)
    );
}
