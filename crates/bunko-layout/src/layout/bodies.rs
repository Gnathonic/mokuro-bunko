//! Text bodies, margins and noise (`line_layout.py:994-1203`).

use std::collections::{BTreeSet, HashMap};

use crate::py::{self, fcmp, max2, median, min2};
use crate::script::{OPENING_BRACKETS, glyph_count, has_kanji};

use super::consts::*;
use super::line::Line;

/// The text body of a novel page (one per tier), in its own deskewed frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub vertical: bool,
    pub theta: f64,
    pub em: f64,
    pub top: f64,
    pub bottom: f64,
    pub cross0: f64,
    pub cross1: f64,
    pub gap: f64,
    /// Raw line indices of the members.
    pub members: BTreeSet<usize>,
}

/// A line's role on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Text,
    Header,
    Footer,
    Noise,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Text => "text",
            Role::Header => "header",
            Role::Footer => "footer",
            Role::Noise => "noise",
        }
    }
}

/// Where the line's first CELL starts, which for a bracket is above its ink.
pub fn text_start(line: &Line, theta: f64, vertical: bool) -> f64 {
    let mut start = line.main_cross(theta, vertical).0;
    let first: String = line.text.chars().take(1).collect();
    if OPENING_BRACKETS.contains(first.as_str()) {
        start -= BRACKET_INK_INSET_EM * line.thickness();
    }
    start
}

/// The outermost edge that at least two lines agree on.
fn supported_edge(values: &[f64], window: f64, lowest: bool) -> f64 {
    let mut ordered = values.to_vec();
    if lowest {
        ordered.sort_by(|a, b| fcmp(*a, *b));
    } else {
        ordered.sort_by(|a, b| fcmp(*b, *a));
    }
    let need = if ordered.len() >= 2 { 2 } else { 1 };
    for &value in &ordered {
        let cluster: Vec<f64> = ordered
            .iter()
            .copied()
            .filter(|v| (v - value).abs() <= window)
            .collect();
        if cluster.len() >= need {
            return median(cluster).unwrap_or(value);
        }
    }
    ordered[0]
}

/// Where the body's FULL columns end: the best-supported end near the last one.
fn full_column_edge(ends: &[f64], window: f64, reach: f64) -> f64 {
    let lowest = py::max_of(ends.iter().copied());
    let near: Vec<f64> = ends
        .iter()
        .copied()
        .filter(|v| lowest - v <= reach)
        .collect();
    let mut best = near[0];
    let mut best_key = (
        near.iter().filter(|u| (*u - best).abs() <= window).count(),
        best,
    );
    for &v in &near[1..] {
        let key = (near.iter().filter(|u| (*u - v).abs() <= window).count(), v);
        if key.0 > best_key.0 || (key.0 == best_key.0 && key.1 > best_key.1) {
            best = v;
            best_key = key;
        }
    }
    median(near.iter().copied().filter(|u| (u - best).abs() <= window)).unwrap_or(best)
}

/// Stretches of the reading axis that many long columns cover at once.
fn coverage_bands(extents: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut events: Vec<(f64, i64)> = extents
        .iter()
        .map(|e| (e.0, 1))
        .chain(extents.iter().map(|e| (e.1, -1)))
        .collect();
    events.sort_by(|a, b| fcmp(a.0, b.0).then(a.1.cmp(&b.1)));
    let (mut peak, mut level) = (0i64, 0i64);
    for &(_, step) in &events {
        level += step;
        peak = peak.max(level);
    }
    let floor = if peak > 2 {
        max2(1.0, BAND_GUTTER_COVERAGE * peak as f64)
    } else {
        0.0
    };
    let mut bands = Vec::new();
    level = 0;
    let mut start: Option<f64> = None;
    for &(position, step) in &events {
        level += step;
        match start {
            None if level as f64 > floor => start = Some(position),
            Some(s) if level as f64 <= floor => {
                bands.push((s, position));
                start = None;
            }
            _ => {}
        }
    }
    bands
}

/// Text bodies of the page: bands of long, aligned, same-size columns.
pub fn find_bodies(lines: &[Line]) -> Vec<Body> {
    let mut bodies = Vec::new();
    for vertical in [true, false] {
        let pool: Vec<&Line> = lines.iter().filter(|l| l.vertical == vertical).collect();
        let long: Vec<&Line> = pool
            .iter()
            .copied()
            .filter(|l| l.length() >= LONG_LINE_EM * l.thickness())
            .collect();
        if long.len() < BODY_MIN_LONG_COLUMNS {
            continue;
        }
        let Some(theta) = median(long.iter().map(|l| l.angle)) else {
            continue;
        };
        let extents: Vec<(f64, f64)> = long
            .iter()
            .map(|l| {
                let s = l.main_cross(theta, vertical);
                (s.0, s.1)
            })
            .collect();
        for (b0, b1) in coverage_bands(&extents) {
            let band: Vec<&Line> = long
                .iter()
                .copied()
                .filter(|l| {
                    let s = l.main_cross(theta, vertical);
                    let mid = py::sum([s.0, s.1]) / 2.0;
                    b0 <= mid && mid <= b1
                })
                .collect();
            if band.len() < BODY_MIN_LONG_COLUMNS {
                continue;
            }
            let Some(em) = median(band.iter().map(|l| l.thickness())) else {
                continue;
            };
            let slack = BODY_EXTENT_SLACK_EM * em;
            let mut members: Vec<&Line> = Vec::new();
            for line in &pool {
                let start = line.main_cross(theta, vertical).0;
                let ratio = max2(line.thickness(), em) / min2(line.thickness(), em);
                if ratio <= BODY_SIZE_RATIO && b0 - slack <= start && start <= b1 {
                    members.push(line);
                }
            }
            if members.len() < BODY_MIN_COLUMNS {
                continue;
            }
            let spans: Vec<_> = members
                .iter()
                .map(|l| l.main_cross(theta, vertical))
                .collect();
            let window = BODY_EDGE_CLUSTER_EM * em;
            let mut by_cross = spans.clone();
            by_cross.sort_by(|a, b| fcmp(a.2, b.2));
            let gaps: Vec<f64> = by_cross
                .windows(2)
                .map(|w| w[1].2 - w[0].3)
                .filter(|g| 0.0 < *g && *g < 2.5 * em)
                .collect();
            let starts: Vec<f64> = members
                .iter()
                .map(|l| text_start(l, theta, vertical))
                .collect();
            let top = min2(
                supported_edge(&starts, window, true),
                py::min_of(band.iter().map(|l| text_start(l, theta, vertical))),
            );
            let ends: Vec<f64> = spans.iter().map(|s| s.1).collect();
            bodies.push(Body {
                vertical,
                theta,
                em,
                top,
                bottom: full_column_edge(&ends, window, BODY_HANGING_REACH_EM * em),
                cross0: py::min_of(spans.iter().map(|s| s.2)),
                cross1: py::max_of(spans.iter().map(|s| s.3)),
                gap: median(gaps).unwrap_or(0.0),
                members: members.iter().map(|l| l.index).collect(),
            });
        }
    }
    bodies.sort_by(|a, b| {
        let ka = if a.vertical { a.top } else { a.cross0 };
        let kb = if b.vertical { b.top } else { b.cross0 };
        fcmp(ka, kb).then(fcmp(a.cross0, b.cross0))
    });
    bodies
}

fn body_y_extent(body: &Body) -> (f64, f64) {
    if body.vertical {
        (body.top, body.bottom)
    } else {
        (body.cross0, body.cross1)
    }
}

fn is_lone_doubt(line: &Line) -> bool {
    glyph_count(&line.text) <= LONE_GLYPHS_MAX
        && !has_kanji(&line.text)
        && line.conf < LONE_GLYPHS_MIN_CONF
}

/// Role of every line (by raw index): text, header, footer or noise.
pub fn classify_roles(
    lines: &[Line],
    bodies: &[Body],
    page_height: f64,
    page_width: f64,
) -> HashMap<usize, Role> {
    let mut roles: HashMap<usize, Role> = lines.iter().map(|l| (l.index, Role::Text)).collect();
    for line in lines {
        if line.conf < LOW_CONFIDENCE {
            roles.insert(line.index, Role::Noise);
        }
    }
    let readable: Vec<&Line> = lines
        .iter()
        .filter(|l| roles[&l.index] == Role::Text)
        .collect();
    if readable.len() == 1 && is_lone_doubt(readable[0]) {
        roles.insert(readable[0].index, Role::Noise);
    }
    if bodies.is_empty() || page_height <= 0.0 {
        return roles;
    }
    let extents: Vec<(f64, f64)> = bodies.iter().map(body_y_extent).collect();
    let y_top = py::min_of(extents.iter().map(|e| e.0));
    let y_bottom = py::max_of(extents.iter().map(|e| e.1));
    if (y_bottom - y_top) < MARGIN_MIN_BODY_COVERAGE * page_height {
        return roles;
    }
    let em = median(bodies.iter().map(|b| b.em)).unwrap_or(0.0);
    let theta = bodies[0].theta;
    let in_body: BTreeSet<usize> = bodies
        .iter()
        .flat_map(|b| b.members.iter().copied())
        .collect();
    for line in lines {
        if roles[&line.index] != Role::Text || in_body.contains(&line.index) {
            continue;
        }
        if line.length() > MARGIN_MAX_LENGTH_FRACTION * page_width {
            continue;
        }
        let (_, _, y0, y1) = line.spans(theta);
        let centre = (y0 + y1) / 2.0;
        let clearance = MARGIN_CLEARANCE_EM * em;
        if y1 <= y_top - clearance && centre <= MARGIN_BAND_FRACTION * page_height {
            roles.insert(line.index, Role::Header);
        } else if y0 >= y_bottom + clearance && centre >= (1.0 - MARGIN_BAND_FRACTION) * page_height
        {
            roles.insert(line.index, Role::Footer);
        }
    }
    roles
}
