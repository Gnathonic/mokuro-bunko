//! Lines -> blocks: column pieces and the merger (`line_layout.py:1211-1411`).

use std::collections::{BTreeMap, HashMap};

use crate::py::{max2, min2};
use crate::records::RawLine;

use super::bodies::{Body, Role, find_bodies};
use super::consts::*;
use super::furigana::filter_furigana;
use super::line::{Line, decide_orientations, measure, measure_lines, overlap, pair_theta};

/// One column the detector cut in two: same size, stacked along the reading axis.
fn is_stitch(ta: f64, tb: f64, cross_overlap: f64, main_gap: f64, body_gap: Option<f64>) -> bool {
    let em = max2(ta, tb);
    let limit = if body_gap.is_none() {
        STITCH_MAX_GAP_EM
    } else {
        BODY_STITCH_MAX_GAP_EM
    };
    em / min2(ta, tb) <= STITCH_MAX_SIZE_RATIO
        && cross_overlap >= STITCH_MIN_CROSS_OVERLAP * min2(ta, tb)
        && main_gap <= limit * em
}

/// Are `a` and `b` two pieces of ONE column (row) the detector cut apart?
pub fn is_column_piece(a: &Line, b: &Line, body_gap: Option<f64>) -> bool {
    let theta = pair_theta(a, b);
    let Some(theta) = theta else {
        return false;
    };
    if a.vertical != b.vertical {
        return false;
    }
    let (am0, am1, ac0, ac1) = a.main_cross(theta, a.vertical);
    let (bm0, bm1, bc0, bc1) = b.main_cross(theta, a.vertical);
    if ac1 <= ac0 || bc1 <= bc0 {
        return false;
    }
    is_stitch(
        ac1 - ac0,
        bc1 - bc0,
        overlap(ac0, ac1, bc0, bc1),
        -overlap(am0, am1, bm0, bm1),
        body_gap,
    )
}

/// Body index of every member line: the first body that lists it.
fn body_of_lines(bodies: &[Body]) -> HashMap<usize, usize> {
    let mut body_of = HashMap::new();
    for (k, body) in bodies.iter().enumerate() {
        for &index in &body.members {
            body_of.entry(index).or_insert(k);
        }
    }
    body_of
}

/// Union-find with path halving and `parent[max(root)] = min(root)`.
struct UnionFind<K: Copy + Ord + std::hash::Hash> {
    parent: HashMap<K, K>,
}

impl<K: Copy + Ord + std::hash::Hash> UnionFind<K> {
    fn find(&mut self, mut i: K) -> K {
        while self.parent[&i] != i {
            let grand = self.parent[&self.parent[&i]];
            self.parent.insert(i, grand);
            i = grand;
        }
        i
    }

    fn union(&mut self, a: K, b: K) {
        let (ra, rb) = (self.find(a), self.find(b));
        self.parent.insert(ra.max(rb), ra.min(rb));
    }
}

/// Groups of raw line indices that are pieces of ONE printed column (row).
///
/// Lines read as nothing take part as pieces of a line with text, in that
/// line's orientation. Groups and members come back in ascending index order.
pub fn column_pieces(raw: &[RawLine]) -> Vec<Vec<usize>> {
    let (mut lines, dropped) = measure_lines(raw);
    decide_orientations(&mut lines);
    let (kept_pos, _) = filter_furigana(&lines);
    let kept: Vec<Line> = kept_pos.iter().map(|&p| lines[p].clone()).collect();
    let mut blanks: Vec<Line> = dropped
        .iter()
        .filter_map(|&i| measure(i, &raw[i]))
        .collect();
    let bodies = find_bodies(&kept);
    let body_of = body_of_lines(&bodies);

    let mut uf = UnionFind {
        parent: kept
            .iter()
            .chain(blanks.iter())
            .map(|l| (l.index, l.index))
            .collect(),
    };
    for (i, a) in kept.iter().enumerate() {
        let body_a = body_of.get(&a.index).copied();
        for b in &kept[i + 1..] {
            let shared = body_a.is_some() && body_a == body_of.get(&b.index).copied();
            let gap = if shared {
                body_a.map(|k| bodies[k].gap)
            } else {
                None
            };
            if is_column_piece(a, b, gap) {
                uf.union(a.index, b.index);
            }
        }
        for blank in &mut blanks {
            blank.vertical = a.vertical;
            if is_column_piece(a, blank, None) {
                uf.union(a.index, blank.index);
            }
        }
    }
    let mut keys: Vec<usize> = uf.parent.keys().copied().collect();
    keys.sort_unstable();
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in keys {
        let root = uf.find(index);
        groups.entry(root).or_default().push(index);
    }
    groups.into_values().filter(|m| m.len() > 1).collect()
}

/// Do two lines belong to the same block? Geometry only.
pub fn should_merge(a: &Line, b: &Line, body_gap: Option<f64>, body_top: Option<f64>) -> bool {
    if a.vertical != b.vertical {
        return false;
    }
    let Some(theta) = pair_theta(a, b) else {
        return false;
    };
    let vertical = a.vertical;
    let (am0, am1, ac0, ac1) = a.main_cross(theta, vertical);
    let (bm0, bm1, bc0, bc1) = b.main_cross(theta, vertical);
    let (ta, tb) = (ac1 - ac0, bc1 - bc0);
    if ta <= 0.0 || tb <= 0.0 {
        return false;
    }
    let em = max2(ta, tb);
    let ratio = em / min2(ta, tb);
    if ratio > MERGE_MAX_SIZE_RATIO {
        return false;
    }
    let gap = -overlap(ac0, ac1, bc0, bc1);
    let mut main_overlap = overlap(am0, am1, bm0, bm1);

    if is_stitch(ta, tb, -gap, -main_overlap, body_gap) {
        return true;
    }
    if let (Some(_), Some(top)) = (body_gap, body_top) {
        main_overlap = overlap(min2(am0, top), am1, min2(bm0, top), bm1);
    }
    if main_overlap < MERGE_MIN_MAIN_OVERLAP * min2(am1 - am0, bm1 - bm0) {
        return false;
    }
    if let Some(bg) = body_gap {
        return gap < max2(MERGE_GAP_EM * em, BODY_GAP_FACTOR * bg);
    }
    let start_diff = (am0 - bm0).abs();
    let end_diff = (am1 - bm1).abs();
    if start_diff > MERGE_STAGGER_EM * em && end_diff > MERGE_STAGGER_EM * em {
        return false;
    }
    let mean_t = (ta + tb) / 2.0;
    let tier1 = if ratio > MERGE_MIXED_SIZE_RATIO {
        MERGE_GAP_MIXED_SIZE_EM
    } else {
        MERGE_GAP_EM
    };
    if gap < tier1 * mean_t {
        return true;
    }
    if ratio < MERGE_ALIGNED_SIZE_RATIO
        && gap < MERGE_ALIGNED_GAP_EM * mean_t
        && start_diff < MERGE_ALIGNED_START_EM * em
    {
        return true;
    }
    gap < MERGE_LOOSE_GAP_EM * mean_t && start_diff < MERGE_LOOSE_START_EM * em
}

/// Group lines into blocks (union-find over `should_merge`); groups are
/// positions into `lines`, ordered by their lowest position.
pub fn merge_lines(
    lines: &[Line],
    bodies: &[Body],
    roles: &HashMap<usize, Role>,
) -> Vec<Vec<usize>> {
    let mut uf = UnionFind {
        parent: (0..lines.len()).map(|i| (i, i)).collect(),
    };
    let body_of = body_of_lines(bodies);
    for (i, a) in lines.iter().enumerate() {
        let role_a = roles.get(&a.index).copied().unwrap_or(Role::Text);
        for (j, b) in lines.iter().enumerate().skip(i + 1) {
            let role_b = roles.get(&b.index).copied().unwrap_or(Role::Text);
            let body_a = body_of.get(&a.index).copied();
            let shared = body_a.is_some() && body_a == body_of.get(&b.index).copied();
            let body_gap = if shared {
                body_a.map(|k| bodies[k].gap)
            } else {
                None
            };
            let body_top = if shared {
                body_a.map(|k| bodies[k].top)
            } else {
                None
            };
            let joined = if role_a == Role::Noise || role_b == Role::Noise {
                let pair = (role_a == Role::Noise && role_b == Role::Text)
                    || (role_a == Role::Text && role_b == Role::Noise);
                pair && is_column_piece(a, b, body_gap)
            } else {
                role_a == role_b && should_merge(a, b, body_gap, body_top)
            };
            if joined {
                let (ri, rj) = (uf.find(i), uf.find(j));
                if ri != rj {
                    uf.parent.insert(ri.max(rj), ri.min(rj));
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..lines.len() {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }
    groups.into_values().collect()
}
