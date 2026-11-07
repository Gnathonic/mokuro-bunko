//! The merge rules (spec §5.9 "Volume groups"): how a losing spelling's folders and
//! volume groups pair with the winner's, and which steps and blocks that plans.
//!
//! Pure over a [`Host`]: every listing, comparison and read goes through it, so each
//! store plans the same way from its own listings.

use std::collections::{HashMap, HashSet};

use crate::relpath::RelPath;

use super::Host;
use super::names::{Named, Part, archive_stems, named, pair_key, split_name, taken_key};
use super::plan::{Block, Defer, Node, Plan, Planned, StepKind, defer};
use super::policy::{CaseMergePolicy, MergeChoice, strip_counter};
use super::scope::last_name;

/// The detail of a file-against-folder block.
const FILE_VS_FOLDER: &str = "a file and a folder whose names differ only in case";

/// The files of one volume group.
type Group<X> = Vec<Named<X>>;

/// One variant set, as its store found it: the spellings, and the nodes behind them.
#[derive(Debug, Clone, Default)]
pub struct SetSpec {
    /// The folder the variants are in.
    pub parent: RelPath,
    /// Same-folder file variants (renames in one folder); otherwise folders.
    pub stem: bool,
    /// A stem set: the [`Named::key`] its files share.
    pub group: String,
    /// The winning spelling, and (a folder set) its member nodes, the move target
    /// first.
    pub winner: (RelPath, Vec<String>),
    /// The losing spellings, each with its member nodes (a folder set; a loser with
    /// none is skipped).
    pub losers: Vec<(RelPath, Vec<String>)>,
    /// A stem set: the parent folder's member nodes, all listed.
    pub parent_ids: Vec<String>,
    /// A stem set: the node its renames land in (`None`: the folder has no node, so
    /// the plan is deferred).
    pub target: Option<String>,
    /// A folder set: the files that fold like the folders, each blocked (file
    /// against folder).
    pub files_vs: Vec<RelPath>,
}

/// Plan one variant set.
pub fn plan_set<H: Host + ?Sized>(
    host: &H,
    policy: &dyn CaseMergePolicy,
    spec: &SetSpec,
) -> Result<Plan, Defer> {
    if spec.stem {
        plan_stem_set(host, policy, spec)
    } else {
        plan_folder_set(host, policy, spec)
    }
}

/// Same-folder file variants: one folder, renames.
pub fn plan_stem_set<H: Host + ?Sized>(
    host: &H,
    policy: &dyn CaseMergePolicy,
    spec: &SetSpec,
) -> Result<Plan, Defer> {
    let mut plan = Plan::default();
    let winner = &spec.winner.0;
    let target = spec
        .target
        .clone()
        .ok_or_else(|| defer("the folder has no node"))?;
    let mut files = Vec::new();
    for m in &spec.parent_ids {
        files.extend(host.list(m)?.into_iter().filter(|n| !n.is_dir));
    }
    let names: Vec<String> = files.iter().map(|n| n.name.clone()).collect();
    let stems = archive_stems(names.iter().map(String::as_str));
    let all: Vec<Named<H::X>> = files.into_iter().map(|n| named(n, &stems)).collect();
    let wstem = split_name(last_name(winner), &stems).0;
    let group: Vec<Named<H::X>> = all
        .iter()
        .filter(|n| n.key == spec.group)
        .cloned()
        .collect();
    let (wg, lg): (Group<H::X>, Group<H::X>) = group.into_iter().partition(|n| n.stem == wstem);
    let taken: HashSet<String> = all.iter().map(|n| taken_key(&n.name)).collect();
    plan.taken.insert(target.clone(), taken);
    if !lg.is_empty() {
        pair(
            host,
            policy,
            &spec.group,
            &wg,
            &lg,
            &target,
            &spec.parent,
            &spec.parent,
            &mut plan,
        )?;
    }
    Ok(plan)
}

/// Folders. A file that folds like them stays (file vs folder).
pub fn plan_folder_set<H: Host + ?Sized>(
    host: &H,
    policy: &dyn CaseMergePolicy,
    spec: &SetSpec,
) -> Result<Plan, Defer> {
    let mut plan = Plan::default();
    let (winner, w_members) = &spec.winner;
    for f in &spec.files_vs {
        plan.blocks.push(Block {
            this: vec![f.clone()],
            that: vec![winner.clone()],
            detail: FILE_VS_FOLDER.into(),
        });
    }
    if w_members.is_empty() {
        return Err(defer("the winning folder has no node"));
    }
    for (l, l_members) in &spec.losers {
        if l_members.is_empty() {
            continue;
        }
        plan_dirs(host, policy, winner, w_members, l, l_members, &mut plan)?;
    }
    Ok(plan)
}

/// `(created_ms, id)`: the order members of one spelling are taken in (the oldest,
/// then the smallest id, is the target).
fn by_age<X>(a: &Node<X>, b: &Node<X>) -> std::cmp::Ordering {
    (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id))
}

/// Merge the folder `l` (members `l_members`) into `w` (members `w_members`,
/// the first is the target), recursively.
pub fn plan_dirs<H: Host + ?Sized>(
    host: &H,
    policy: &dyn CaseMergePolicy,
    w: &RelPath,
    w_members: &[String],
    l: &RelPath,
    l_members: &[String],
    plan: &mut Plan,
) -> Result<(), Defer> {
    let target = w_members[0].clone();
    let mut w_kids = Vec::new();
    for m in w_members {
        w_kids.extend(host.list(m)?);
    }
    let mut l_kids = Vec::new();
    for m in l_members {
        let kids = host.list(m)?;
        plan.empty_loser |= kids.is_empty();
        l_kids.extend(kids);
    }
    let taken: HashSet<String> = w_kids.iter().map(|n| taken_key(&n.name)).collect();
    plan.taken.entry(target.clone()).or_default().extend(taken);
    // Folders: same fold → recursive; else move in whole.
    let mut w_dirs: HashMap<String, Vec<Node<H::X>>> = HashMap::new();
    for n in w_kids.iter().filter(|n| n.is_dir) {
        w_dirs.entry(pair_key(&n.name)).or_default().push(n.clone());
    }
    for v in w_dirs.values_mut() {
        v.sort_by(by_age);
    }
    let w_file_folds: HashMap<String, String> = w_kids
        .iter()
        .filter(|n| !n.is_dir)
        .map(|n| (pair_key(&n.name), n.name.clone()))
        .collect();
    let mut l_dirs: Vec<(String, Vec<Node<H::X>>)> = Vec::new();
    for n in l_kids.iter().filter(|n| n.is_dir) {
        let name = n.name.clone();
        match l_dirs.iter_mut().find(|(k, _)| *k == name) {
            Some((_, v)) => v.push(n.clone()),
            None => l_dirs.push((name, vec![n.clone()])),
        }
    }
    for (name, nodes) in l_dirs {
        let f = pair_key(&name);
        let Some(lsub) = l.join(name.clone()) else {
            continue;
        };
        if let Some(wds) = w_dirs.get(&f) {
            let wname = wds
                .iter()
                .find(|n| n.name == name)
                .map_or_else(|| wds[0].name.clone(), |n| n.name.clone());
            let mut sub: Vec<&Node<H::X>> = wds.iter().filter(|n| n.name == wname).collect();
            sub.sort_by(|a, b| by_age(a, b));
            let sub_members: Vec<String> = sub.iter().map(|n| n.id.clone()).collect();
            let Some(wsub) = w.join(wname) else { continue };
            let mut lsorted = nodes.clone();
            lsorted.sort_by(by_age);
            let l_sub: Vec<String> = lsorted.iter().map(|n| n.id.clone()).collect();
            plan_dirs(host, policy, &wsub, &sub_members, &lsub, &l_sub, plan)?;
        } else if let Some(wf) = w_file_folds.get(&f) {
            plan.blocks.push(Block {
                this: vec![lsub],
                that: w.join(wf.clone()).into_iter().collect(),
                detail: FILE_VS_FOLDER.into(),
            });
        } else {
            let Some(dest) = w.join(name.clone()) else {
                continue;
            };
            for n in &nodes {
                plan.steps.push(Planned::mv(
                    &format!("{l}/{f}"),
                    n,
                    lsub.clone(),
                    dest.clone(),
                    &target,
                ));
            }
            plan.taken
                .entry(target.clone())
                .or_default()
                .insert(taken_key(&name));
        }
    }
    // Files, as volume groups by folded stem.
    let w_files: Vec<Node<H::X>> = w_kids.iter().filter(|n| !n.is_dir).cloned().collect();
    let l_files: Vec<Node<H::X>> = l_kids.iter().filter(|n| !n.is_dir).cloned().collect();
    let names: Vec<String> = w_files
        .iter()
        .chain(&l_files)
        .map(|n| n.name.clone())
        .collect();
    let stems = archive_stems(names.iter().map(String::as_str));
    let wn: Vec<Named<H::X>> = w_files.into_iter().map(|n| named(n, &stems)).collect();
    let ln: Vec<Named<H::X>> = l_files.into_iter().map(|n| named(n, &stems)).collect();
    let w_dir_folds: HashSet<&String> = w_dirs.keys().collect();
    let mut keys: Vec<String> = Vec::new();
    for n in &ln {
        if !keys.contains(&n.key) {
            keys.push(n.key.clone());
        }
    }
    for key in keys {
        let lg: Vec<Named<H::X>> = ln.iter().filter(|n| n.key == key).cloned().collect();
        if let Some(f) = lg.iter().find(|n| w_dir_folds.contains(&pair_key(&n.name))) {
            plan.blocks.push(Block {
                this: lg.iter().filter_map(|n| l.join(n.name.clone())).collect(),
                that: w.join(f.name.clone()).into_iter().collect(),
                detail: FILE_VS_FOLDER.into(),
            });
            continue;
        }
        let wg: Vec<Named<H::X>> = wn.iter().filter(|n| n.key == key).cloned().collect();
        pair(host, policy, &key, &wg, &lg, &target, w, l, plan)?;
    }
    for m in l_members {
        plan.finishes.push(Planned {
            kind: StepKind::Finish,
            vg: format!("{l}/"),
            id: m.clone(),
            ver: None,
            // Where Bunko's own writes still queued for the loser go instead.
            src: Some(w.clone()),
            dest: l.clone(),
            parent_id: None,
            is_dir: true,
            hooks: false,
            bytes: None,
            series: None,
            expect_dup: false,
        });
    }
    Ok(())
}

/// The next free `<stem>.<id>.mokuro` in `target` starting from `base` (`base`
/// itself first, then `-2`, `-3`, …), free under [`taken_key`].
pub fn free_layer(plan: &mut Plan, target: &str, stem: &str, base: &str, ext: &str) -> String {
    let taken = plan.taken.entry(target.to_string()).or_default();
    let base = strip_counter(base).to_string();
    let mut n = 1u32;
    loop {
        let id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let name = format!("{stem}.{id}{ext}");
        if !taken.contains(&taken_key(&name)) {
            taken.insert(taken_key(&name));
            return name;
        }
        n += 1;
    }
}

/// Pair one loser volume group with the winner's of the same folded stem
/// (§5.9 Volume groups).
#[allow(clippy::too_many_arguments)]
pub fn pair<H: Host + ?Sized>(
    host: &H,
    policy: &dyn CaseMergePolicy,
    key: &str,
    wg: &[Named<H::X>],
    lg: &[Named<H::X>],
    target: &str,
    w: &RelPath,
    l: &RelPath,
    plan: &mut Plan,
) -> Result<(), Defer> {
    let vg = format!("{l}/{key}");
    let lpath = |n: &Named<H::X>| l.join(n.name.clone());
    let wpath = |n: &Named<H::X>| w.join(n.name.clone());
    let block = |plan: &mut Plan, detail: String| {
        plan.blocks.push(Block {
            this: lg.iter().filter_map(lpath).collect(),
            that: wg.iter().filter_map(wpath).collect(),
            detail,
        });
    };
    let series_dir = w.segments().len() == 1;
    let is_series = |n: &Named<H::X>| {
        series_dir && n.part == Part::Other && crate::names::is_fixed_name(&n.name, "series.json")
    };
    let wstem = wg
        .iter()
        .find(|n| n.part == Part::Archive)
        .or_else(|| wg.first())
        .map(|n| n.stem.clone());
    let mut lsorted: Vec<&Named<H::X>> = lg.iter().collect();
    lsorted.sort_by_key(|n| n.part != Part::Archive);
    let (mut moves, mut l_trash, mut submits) = (Vec::new(), Vec::new(), Vec::new());
    let mut archives_identical = false;
    for lf in lsorted {
        let Some(lrel) = lpath(lf) else { continue };
        if is_series(lf) {
            let Some(series) = w.join("series.json") else {
                continue;
            };
            submits.push(Planned {
                kind: StepKind::Submit,
                vg: vg.clone(),
                id: lf.node.id.clone(),
                ver: Some(lf.node.ver.clone()),
                src: None,
                dest: lrel,
                parent_id: None,
                is_dir: false,
                hooks: true,
                bytes: None,
                series: Some(series),
                expect_dup: false,
            });
            continue;
        }
        let counterpart = wg
            .iter()
            .find(|wf| pair_key(&wf.suffix) == pair_key(&lf.suffix) && wf.part == lf.part);
        let Some(wf) = counterpart else {
            // Only the loser has it: in it goes, under the winner's stem.
            let name = match &wstem {
                Some(s) => format!("{s}{}", lf.suffix),
                None => lf.name.clone(),
            };
            let Some(dest) = w.join(name.clone()) else {
                continue;
            };
            plan.taken
                .entry(target.to_string())
                .or_default()
                .insert(taken_key(&name));
            moves.push(Planned::mv(&vg, &lf.node, lrel, dest, target));
            continue;
        };
        let Some(wrel) = wpath(wf) else { continue };
        if host.identical(&wf.node, &lf.node)? {
            if lf.part == Part::Archive {
                archives_identical = true;
            }
            l_trash.push(Planned::trash(&vg, &lf.node, lrel));
            continue;
        }
        match &lf.part {
            Part::Archive => {
                block(plan, format!("{lrel} and {wrel} are different archives"));
                return Ok(());
            }
            Part::Cover => l_trash.push(Planned::trash(&vg, &lf.node, lrel)),
            Part::Main => {
                if !archives_identical || pair_key(&lf.suffix).ends_with(".gz") {
                    block(
                        plan,
                        format!("{lrel} and {wrel} hold different OCR for this volume"),
                    );
                    return Ok(());
                }
                let choice = policy.prefer_main(
                    &host.path_of(&wf.node, &wrel),
                    &host.path_of(&lf.node, &lrel),
                );
                if choice == MergeChoice::Block {
                    block(
                        plan,
                        format!(
                            "{lrel} and {wrel} hold different OCR, and neither (or both) \
                             matches the primary OCR setting"
                        ),
                    );
                    return Ok(());
                }
                // No copy is kept (owner decision, 2026-11-01). A: the loser's main
                // is trashed below, like any other superseded file. B: its bytes
                // replace the winner's in place and the loser's node is trashed;
                // where the provider's replace makes a new node, the winner's old
                // node goes to the trash too (the store's executor).
                if choice == MergeChoice::B {
                    // The loser's main replaces the winner's in place: a new version
                    // of the winner's node, based on the version judged here. The
                    // winner's stays untouched until that replace has committed; a
                    // 412 or any failure blocks the group instead.
                    let new_bytes = host.replace_bytes(&lf.node)?;
                    moves.push(Planned {
                        kind: StepKind::Replace,
                        vg: vg.clone(),
                        id: wf.node.id.clone(),
                        ver: Some(wf.node.ver.clone()),
                        src: Some(lrel.clone()),
                        dest: wrel.clone(),
                        parent_id: Some(target.to_string()),
                        is_dir: false,
                        hooks: true,
                        bytes: new_bytes,
                        series: None,
                        expect_dup: false,
                    });
                }
                l_trash.push(Planned::trash(&vg, &lf.node, lrel));
            }
            Part::Layer(id) if policy.is_unstamped_layer(id) => {
                let stem = wstem.clone().unwrap_or_else(|| wf.stem.clone());
                let ext = &lf.suffix[1 + id.len()..];
                let name = free_layer(plan, target, &stem, id, ext);
                let Some(dest) = w.join(name) else { continue };
                moves.push(Planned::mv(&vg, &lf.node, lrel, dest, target));
            }
            Part::Layer(_) | Part::Other => {
                block(plan, format!("{lrel} and {wrel} differ"));
                return Ok(());
            }
        }
    }
    plan.steps.extend(moves);
    plan.submits.extend(submits);
    plan.l_trash.extend(l_trash);
    Ok(())
}

/// Order the files of one same-folder stem set and keep one per exact stem: the
/// archives' first, then the oldest (`created_ms`), then the smallest id, by `key`;
/// the first is the winner's spelling. An archive's spelling so anchors its volume.
pub fn order_stem_members<T, K: Ord>(
    v: &mut Vec<T>,
    stems: &HashSet<String>,
    name: impl Fn(&T) -> &str,
    key: impl Fn(&T) -> K,
) {
    let archive = |e: &T| split_name(name(e), stems).2 == Part::Archive;
    v.sort_by_key(|e| (!archive(e), key(e)));
    let mut stems_seen = HashSet::new();
    v.retain(|e| stems_seen.insert(split_name(name(e), stems).0));
}
