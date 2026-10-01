//! Line order, paragraphs and block reading order (`line_layout.py:1419-1594`).

use crate::py::{self, fcmp, median};
use crate::script::{OPENING_BRACKETS, SENTENCE_ENDINGS, glyph_count};

use super::bodies::{Body, text_start};
use super::consts::*;
use super::line::{Line, Spans, overlap};

/// The kind of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Body,
    Text,
    Header,
    Footer,
    Noise,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Body => "body",
            Kind::Text => "text",
            Kind::Header => "header",
            Kind::Footer => "footer",
            Kind::Noise => "noise",
        }
    }
}

/// Frame of a block: the median angle of its lines that have one.
pub fn block_theta(group: &[&Line]) -> f64 {
    median(group.iter().filter(|l| l.angle_reliable()).map(|l| l.angle)).unwrap_or(0.0)
}

/// The block's lines as columns (rows) in reading order.
pub fn cluster_columns<'a>(group: &[&'a Line], theta: Option<f64>) -> Vec<Vec<&'a Line>> {
    if group.is_empty() {
        return Vec::new();
    }
    let theta = theta.unwrap_or_else(|| block_theta(group));
    let vertical = group[0].vertical;
    let flow = if vertical { -1.0 } else { 1.0 };
    let mut keyed: Vec<(f64, f64, usize, &'a Line)> = group
        .iter()
        .map(|l| {
            let (m0, _, c0, c1) = l.main_cross(theta, vertical);
            (flow * (c0 + c1) / 2.0, m0, l.index, *l)
        })
        .collect();
    keyed.sort_by(|a, b| fcmp(a.0, b.0).then(fcmp(a.1, b.1)).then(a.2.cmp(&b.2)));
    let mut columns: Vec<Vec<(f64, f64, usize, &'a Line)>> = Vec::new();
    for item in keyed {
        let joins = columns
            .last()
            .is_some_and(|col| (item.0 - col[0].0).abs() <= COLUMN_CLUSTER_EM * item.3.thickness());
        if joins {
            if let Some(col) = columns.last_mut() {
                col.push(item);
            }
        } else {
            columns.push(vec![item]);
        }
    }
    columns
        .into_iter()
        .map(|mut col| {
            col.sort_by(|a, b| fcmp(a.1, b.1).then(a.2.cmp(&b.2)));
            col.into_iter().map(|it| it.3).collect()
        })
        .collect()
}

/// Lines of one block in reading order.
pub fn order_lines<'a>(group: &[&'a Line]) -> Vec<&'a Line> {
    cluster_columns(group, None).into_iter().flatten().collect()
}

fn first_char(s: &str) -> String {
    s.chars().take(1).collect()
}

fn last_char(s: &str) -> String {
    s.chars().last().map(String::from).unwrap_or_default()
}

/// Split a merged body block at paragraph starts; lines come back ordered.
pub fn split_paragraphs<'a>(group: &[&'a Line], body: &Body) -> Vec<Vec<&'a Line>> {
    let columns = cluster_columns(group, Some(body.theta));
    let mut paragraphs: Vec<Vec<&'a Line>> = Vec::new();
    let (mut prev_inset, mut prev_short) = (0.0f64, 0.0f64);
    let mut prev_text = String::new();
    for (k, column) in columns.into_iter().enumerate() {
        let inset = (text_start(column[0], body.theta, body.vertical) - body.top) / body.em;
        let short = (body.bottom - py::max_of(column.iter().map(|l| l.main_cross(body.theta, body.vertical).1))) / body.em;
        let text: String = column.iter().map(|l| l.text.as_str()).collect();
        let start = if k == 0 || prev_short >= PARAGRAPH_SHORT_END_EM {
            true
        } else if inset >= PARAGRAPH_DEEP_INSET_EM {
            (inset - prev_inset).abs() > PARAGRAPH_INDENT_MIN_EM
        } else if inset >= PARAGRAPH_INDENT_MIN_EM {
            true
        } else {
            OPENING_BRACKETS.contains(first_char(&text).as_str())
                && prev_short >= PARAGRAPH_SOFT_END_EM
                && SENTENCE_ENDINGS.contains(last_char(&prev_text).as_str())
        };
        if start || paragraphs.is_empty() {
            paragraphs.push(Vec::new());
        }
        if let Some(p) = paragraphs.last_mut() {
            p.extend(column);
        }
        prev_inset = inset;
        prev_short = short;
        prev_text = text;
    }
    paragraphs
}

/// `(x0, x1, y0, y1)` over the members' axis-aligned spans.
pub fn group_box(group: &[&Line]) -> Spans {
    let spans: Vec<Spans> = group.iter().map(|l| l.spans(0.0)).collect();
    (
        py::min_of(spans.iter().map(|s| s.0)),
        py::max_of(spans.iter().map(|s| s.1)),
        py::min_of(spans.iter().map(|s| s.2)),
        py::max_of(spans.iter().map(|s| s.3)),
    )
}

/// Reading order of the blocks, as indices into `groups`.
pub fn order_blocks(groups: &[Vec<&Line>], kinds: &[Kind], bodies: &[Body]) -> Vec<usize> {
    let boxes: Vec<Spans> = groups.iter().map(|g| group_box(g)).collect();
    let of_kind = |k: Kind| -> Vec<usize> { (0..kinds.len()).filter(|&i| kinds[i] == k).collect() };
    let flow: Vec<usize> = (0..kinds.len()).filter(|&i| matches!(kinds[i], Kind::Text | Kind::Body)).collect();
    let mut order = order_rows(&of_kind(Kind::Header), groups, &boxes);

    let mut cuts: Vec<f64> = Vec::new();
    let stacked: Vec<&Body> = bodies.iter().filter(|b| b.vertical).collect();
    for w in stacked.windows(2) {
        let (upper, lower) = (w[0], w[1]);
        let shared = overlap(upper.cross0, upper.cross1, lower.cross0, lower.cross1);
        let narrower = py::min2(upper.cross1 - upper.cross0, lower.cross1 - lower.cross0);
        if lower.top > upper.bottom && shared >= 0.5 * narrower {
            cuts.push((upper.bottom + lower.top) / 2.0);
        }
    }
    let mut tiers: Vec<Vec<usize>> = vec![Vec::new(); cuts.len() + 1];
    for i in flow {
        let centre = (boxes[i].2 + boxes[i].3) / 2.0;
        tiers[cuts.iter().filter(|&&cut| centre > cut).count()].push(i);
    }
    for tier in &tiers {
        order.extend(order_rows(tier, groups, &boxes));
    }
    order.extend(order_rows(&of_kind(Kind::Footer), groups, &boxes));
    order.extend(order_rows(&of_kind(Kind::Noise), groups, &boxes));
    order
}

/// Vertical overlap of two blocks as a share of the shorter one.
fn row_overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    overlap(a0, a1, b0, b1) / py::max2(py::min2(a1 - a0, b1 - b0), 1e-6)
}

fn order_rows(indices: &[usize], groups: &[Vec<&Line>], boxes: &[Spans]) -> Vec<usize> {
    let mut remaining = indices.to_vec();
    remaining.sort_by(|&a, &b| fcmp(boxes[a].2, boxes[b].2).then(fcmp(-boxes[a].1, -boxes[b].1)).then(a.cmp(&b)));
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let seed = remaining[0];
        let (s0, s1) = (boxes[seed].2, boxes[seed].3);
        let mut row: Vec<usize> = Vec::new();
        for &i in &remaining {
            let (y0, y1) = (boxes[i].2, boxes[i].3);
            let joins = i == seed
                || row_overlap(s0, s1, y0, y1) >= ROW_MIN_OVERLAP
                || row.iter().any(|&m| row_overlap(boxes[m].2, boxes[m].3, y0, y1) >= ROW_CHAIN_OVERLAP);
            if joins {
                row.push(i);
            }
        }
        let glyphs_vertical: i64 = row
            .iter()
            .flat_map(|&i| groups[i].iter())
            .map(|l| glyph_count(&l.text) as i64 * if l.vertical { 1 } else { -1 })
            .sum();
        if glyphs_vertical >= 0 {
            row.sort_by(|&a, &b| fcmp(-boxes[a].1, -boxes[b].1).then(fcmp(boxes[a].2, boxes[b].2)).then(a.cmp(&b)));
        } else {
            row.sort_by(|&a, &b| fcmp(boxes[a].0, boxes[b].0).then(fcmp(boxes[a].2, boxes[b].2)).then(a.cmp(&b)));
        }
        ordered.extend(row.iter().copied());
        remaining.retain(|i| !row.contains(i));
    }
    ordered
}
