//! The reconciled road's pure half (`engine_runner.py` `ReconciledPageReader`,
//! spec §7.2): which lines the engine reads, their glyph room, the merge of
//! its reads with the CTC reads, the engine-only drop rule, the seam trim,
//! and the raw dump.
//!
//! The engine itself (crops + recognizer) lives elsewhere, so the road is a
//! sequence of calls around two engine batches:
//!
//! ```text
//! let road = EngineRoad::plan(&lines, &first_layout);     // first = layout of the rounded page
//! let texts = engine.read(road.targets, road.token_caps()); // first batch, crop_fn
//! let mut settled = road.reconcile_first(&lines, &texts);
//! let doubted = road.doubted(&settled);                     // paddle-manga only
//! let second = engine.read(doubted, ...);                   // second_crop_fn
//! road.settle_second(&mut settled, &doubted, &second);
//! road.apply(&mut lines, &mut settled, width, height);      // verdicts + seam trim
//! let done = road.finish(&lines, &settled, width, height, detector, version);
//! ```

use std::collections::{BTreeSet, HashMap};

use crate::json::Value;
use crate::layout::{PageLayout, column_pieces};
use crate::py::{self, hypot, round_digits};
use crate::reconcile::{
    CONFIRMED_CONF, Reconciled, engine_only_verdict, line_cells, needs_second_read, overlap_repeat,
    page_summary, reconcile_line, settle_disputes, token_cap,
};
use crate::records::{RawLine, RawPage};
use crate::script::normalize_text;
use crate::sidecar::{FinishedPage, finish_page};

/// `engine_runner.UPRIGHT_MARGIN`.
pub const UPRIGHT_MARGIN: f64 = 0.12;
/// `engine_runner.LINE_MARGIN_EM`.
pub const LINE_MARGIN_EM: f64 = 0.25;
/// `engine_runner.SECOND_MARGIN_EM` (paddle-manga's second crop).
pub const SECOND_MARGIN_EM: f64 = 0.5;
pub const PITCH_MIN_CONF: f64 = 0.5;
pub const PITCH_MIN_LINES: usize = 3;
pub const NEIGHBOUR_ANGLE: f64 = 15.0;
pub const NEIGHBOUR_REACH: f64 = 3.0;

fn pts(quad: &[[f64; 2]]) -> Vec<(f64, f64)> {
    quad.iter().take(4).map(|p| (p[0], p[1])).collect()
}

/// `(main, cross)` extents of a line quad, the way the reader computes them
/// (`(dx**2 + dy**2) ** 0.5`, i.e. libm `pow`, not `hypot`).
pub fn quad_extents(quad: &[[f64; 2]], vertical: bool) -> (f64, f64) {
    let p = pts(quad);
    if p.len() < 4 {
        return (0.0, 0.0);
    }
    let mid: Vec<(f64, f64)> = (0..4)
        .map(|i| {
            (
                (p[i].0 + p[(i + 1) % 4].0) / 2.0,
                (p[i].1 + p[(i + 1) % 4].1) / 2.0,
            )
        })
        .collect();
    let vec_v = (mid[2].0 - mid[0].0, mid[2].1 - mid[0].1);
    let vec_h = (mid[1].0 - mid[3].0, mid[1].1 - mid[3].1);
    let len_v = py::pow(py::pow(vec_v.0, 2.0) + py::pow(vec_v.1, 2.0), 0.5);
    let len_h = py::pow(py::pow(vec_h.0, 2.0) + py::pow(vec_h.1, 2.0), 0.5);
    if vertical {
        (len_v, len_h)
    } else {
        (len_h, len_v)
    }
}

/// Margin of a line crop in pixels.
pub fn line_margin_px(quad: &[[f64; 2]], margin_em: f64) -> f64 {
    let (main, cross) = quad_extents(quad, true);
    py::min2(
        UPRIGHT_MARGIN * py::max2(main, cross),
        margin_em * py::min2(main, cross),
    )
}

/// The page's glyph cell: median thickness of the lines the CTC read text in.
pub fn body_pitch(lines: &[RawLine]) -> f64 {
    let thickness: Vec<f64> = lines
        .iter()
        .filter(|l| !py::strip(&l.text).is_empty() && l.conf >= PITCH_MIN_CONF)
        .map(|l| quad_extents(&l.quad, l.vertical).1)
        .filter(|t| *t > 0.0)
        .collect();
    if thickness.len() < PITCH_MIN_LINES {
        return 0.0;
    }
    py::median(thickness).unwrap_or(0.0)
}

/// Per line: how many lines the CTC recognizer READ run parallel to it nearby.
pub fn parallel_neighbours(lines: &[RawLine]) -> Vec<usize> {
    let geo: Vec<(f64, f64, f64)> = lines
        .iter()
        .map(|l| {
            let thick = quad_extents(&l.quad, l.vertical).1;
            let p = pts(&l.quad);
            let count = p.len().max(1) as f64;
            (
                thick,
                py::sum(p.iter().map(|q| q.0)) / count,
                py::sum(p.iter().map(|q| q.1)) / count,
            )
        })
        .collect();
    let read: Vec<bool> = lines
        .iter()
        .map(|l| !py::strip(&l.text).is_empty())
        .collect();
    (0..lines.len())
        .map(|i| {
            let (thick, cx, cy) = geo[i];
            (0..lines.len())
                .filter(|&j| {
                    if j == i || !read[j] || lines[j].vertical != lines[i].vertical {
                        return false;
                    }
                    if (lines[j].angle - lines[i].angle).abs() > NEIGHBOUR_ANGLE {
                        return false;
                    }
                    let (other_thick, ox, oy) = geo[j];
                    hypot(cx - ox, cy - oy)
                        <= NEIGHBOUR_REACH * py::max_of([thick, other_thick, 1.0])
                })
                .count()
        })
        .collect()
}

/// What the engine reads on one page, measured once.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRoad {
    /// Line indices the engine reads (every line that is not ruby).
    pub targets: Vec<usize>,
    /// Lines of a text body (the `thin` glyph rule applies there).
    pub in_body: BTreeSet<usize>,
    /// The page's body pitch, 0.0 when it has none.
    pub pitch: f64,
    /// Per line (all lines): parallel read neighbours.
    pub neighbours: Vec<usize>,
    /// Per target: glyph cells of its quad.
    pub cells: Vec<i64>,
}

impl EngineRoad {
    /// `lines` are the reader's unrounded lines; `first` is
    /// `layout_page(RawPage::rounded(...))` of those same lines.
    pub fn plan(lines: &[RawLine], first: &PageLayout) -> EngineRoad {
        let skip = first.ruby_lines();
        let targets: Vec<usize> = (0..lines.len()).filter(|i| !skip.contains(i)).collect();
        let pitch = body_pitch(lines);
        let cells = targets
            .iter()
            .map(|&i| {
                let (main, cross) = quad_extents(&lines[i].quad, lines[i].vertical);
                line_cells(main, cross, pitch)
            })
            .collect();
        EngineRoad {
            targets,
            in_body: first.body_members(),
            pitch,
            neighbours: parallel_neighbours(lines),
            cells,
        }
    }

    /// `max_new_tokens` per target (for recognizers that take token caps).
    pub fn token_caps(&self) -> Vec<i64> {
        self.cells.iter().map(|c| token_cap(*c)).collect()
    }

    /// Merge the engine's first reads (`texts[k]` for target `k`, the crops of
    /// a line concatenated) with the CTC reads.
    pub fn reconcile_first(&self, lines: &[RawLine], texts: &[String]) -> Vec<Reconciled> {
        self.targets
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                let line = &lines[i];
                reconcile_line(
                    texts.get(k).map(String::as_str).unwrap_or(""),
                    &normalize_text(py::strip(&line.text)),
                    self.cells[k],
                    self.in_body.contains(&i),
                    Some(line.conf),
                    Some(&line.char_confs),
                )
            })
            .collect()
    }

    /// Targets (positions `k`) worth a second engine read.
    pub fn doubted(&self, settled: &[Reconciled]) -> Vec<usize> {
        (0..settled.len())
            .filter(|&k| needs_second_read(&settled[k]))
            .collect()
    }

    /// Apply the second reads: `second[n]` is the read of target `doubted[n]`.
    pub fn settle_second(&self, settled: &mut [Reconciled], doubted: &[usize], second: &[String]) {
        for (n, &k) in doubted.iter().enumerate() {
            let text = second.get(n).map(String::as_str).unwrap_or("");
            settled[k] = settle_disputes(settled[k].clone(), text, self.cells[k]);
        }
    }

    /// Write the settled texts into the lines: the engine-only verdicts (a
    /// dropped line keeps its CTC text), then the seam trim between pieces of
    /// one column that stayed apart.
    pub fn apply(
        &self,
        lines: &mut [RawLine],
        settled: &mut [Reconciled],
        width: i64,
        height: i64,
    ) {
        for (k, &i) in self.targets.iter().enumerate() {
            let result = &mut settled[k];
            let (main, em) = quad_extents(&lines[i].quad, lines[i].vertical);
            if result.engine_only {
                let (keep, why) = engine_only_verdict(
                    result,
                    self.cells[k],
                    lines[i].score,
                    main,
                    em,
                    self.pitch,
                    self.neighbours[i],
                );
                result.notes.push(why.to_string());
                if !keep {
                    result.text = normalize_text(py::strip(&lines[i].text));
                    result.notes.push("dropped".to_string());
                    continue;
                }
                lines[i].text = result.text.clone();
                lines[i].conf = py::max2(lines[i].conf, CONFIRMED_CONF);
                continue;
            }
            lines[i].text = result.text.clone();
        }
        self.trim_repeats(lines, settled, width, height);
    }

    fn trim_repeats(
        &self,
        lines: &mut [RawLine],
        settled: &mut [Reconciled],
        width: i64,
        height: i64,
    ) {
        let raw = RawPage::new(width, height, lines.to_vec()).rounded();
        let by_line: HashMap<usize, usize> = self
            .targets
            .iter()
            .enumerate()
            .map(|(k, &i)| (i, k))
            .collect();
        for group in column_pieces(&raw.lines) {
            let vertical = lines[group[0]].vertical;
            let axis = if vertical { 1 } else { 0 };
            let mut spans: Vec<(f64, f64, usize)> = group
                .iter()
                .map(|&i| {
                    let q = pts(&lines[i].quad);
                    (
                        py::min_of(q.iter().map(|p| if axis == 1 { p.1 } else { p.0 })),
                        py::max_of(q.iter().map(|p| if axis == 1 { p.1 } else { p.0 })),
                        i,
                    )
                })
                .collect();
            spans.sort_by(|a, b| {
                py::fcmp(a.0, b.0)
                    .then(py::fcmp(a.1, b.1))
                    .then(a.2.cmp(&b.2))
            });
            for w in 0..spans.len().saturating_sub(1) {
                let (_, end, before) = spans[w];
                let (start, _, after) = spans[w + 1];
                let (Some(_), Some(&ka)) = (by_line.get(&before), by_line.get(&after)) else {
                    continue;
                };
                let quad = lines[after].quad.clone();
                let (_main, em) = quad_extents(&quad, vertical);
                let shared = (end - start) + 2.0 * line_margin_px(&quad, LINE_MARGIN_EM);
                if shared <= 0.0 || em <= 0.0 {
                    continue;
                }
                let room = (shared / em + 0.5).trunc() as i64 + 1;
                let count = overlap_repeat(&lines[before].text, &lines[after].text, room);
                if count > 0 {
                    let trimmed: String =
                        py::strip(&lines[after].text).chars().skip(count).collect();
                    lines[after].text = trimmed.clone();
                    settled[ka].text = trimmed;
                    settled[ka].notes.push("seam".to_string());
                }
            }
        }
    }

    /// Lay the merged lines out and build the raw dump with the per-line
    /// reconcile record and the page tally (`finish_read`).
    pub fn finish(
        &self,
        lines: &[RawLine],
        settled: &[Reconciled],
        width: i64,
        height: i64,
        detector: Option<Value>,
        version: &str,
    ) -> FinishedPage {
        let mut done = finish_page(lines, width, height, detector, version);
        if let Some(Value::Array(raw_lines)) = done.raw.get_mut("lines") {
            for (k, &i) in self.targets.iter().enumerate() {
                if let (Some(entry), Value::Object(extra)) =
                    (raw_lines.get_mut(i), settled[k].to_value())
                {
                    for (key, v) in extra {
                        entry.set(&key, v);
                    }
                }
            }
        }
        let mut tally = page_summary(settled);
        tally.set("body_pitch", Value::Float(round_digits(self.pitch, 1)));
        done.raw.set("reconcile", tally);
        done.doubtful = Some(doubtful_lines(&done.raw));
        done
    }
}

/// Lines of a reconciled page an editor should look at first (`review.json`).
pub fn doubtful_lines(raw: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(lines) = raw.get("lines").and_then(Value::as_array) else {
        return out;
    };
    for (index, line) in lines.iter().enumerate() {
        let agreement = line.get("agreement").cloned().unwrap_or(Value::Null);
        let notes = line.get("notes").cloned().unwrap_or(Value::Null);
        let fell_back = line.get("source").and_then(Value::as_str) == Some("ctc")
            && notes
                .as_array()
                .is_some_and(|ns| ns.iter().any(|n| n.as_str() == Some("runaway")));
        let below =
            !matches!(agreement, Value::Null) && agreement.as_f64().is_some_and(|a| a < 1.0);
        if !line.get("text").is_some_and(Value::truthy) || !(fell_back || below) {
            continue;
        }
        let mut entry = vec![("line".to_string(), Value::Int(index as i64))];
        for key in ["quad", "text", "ctc", "vlm"] {
            if let Some(v) = line.get(key) {
                entry.push((key.to_string(), v.clone()));
            }
        }
        if let Some(v) = line.get("vlm_second") {
            entry.push(("vlm_second".to_string(), v.clone()));
        }
        entry.push(("agreement".to_string(), agreement));
        entry.push(("notes".to_string(), notes));
        out.push(Value::Object(entry));
    }
    out
}
