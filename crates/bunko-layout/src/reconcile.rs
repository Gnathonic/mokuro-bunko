//! `line_reconcile.py`: an engine (VLM) read of a line merged with the CTC read.
//!
//! The engine's characters win; the CTC read gives back the printed widths,
//! brackets, blank cells and dashes the engine cannot write, and stands in
//! when the engine ran away or said nothing (spec §7.3). The alignment is
//! [`crate::difflib`], an exact port of Python's, so the opcodes - and so the
//! merged text - are the same.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use crate::difflib::{Opcode, SequenceMatcher, Tag};
use crate::json::Value;
use crate::py::{self, is_ascii_alnum, is_space, round_digits};

pub const OPENERS: &str = "「『（〈《【〔";
pub const CLOSERS: &str = "」』）〉》】〕。、";
pub const ANCHOR_GLYPHS: usize = 2;
pub const PATCH_MIN_AGREEMENT: f64 = 0.8;
pub const RUNAWAY_RATIO: f64 = 1.35;
pub const RUNAWAY_SLACK: f64 = 2.0;
pub const REGION_MIN_PITCHES: f64 = 2.0;
pub const ROOM_MIN_CELLS: usize = 4;
pub const REPEAT_MAX_UNIT: usize = 6;
pub const REPEAT_MIN_COUNT: usize = 4;
pub const LOOP_RATIO: f64 = 2.0;
pub const LOOP_MIN_SHARE: f64 = 0.8;
pub const TOKENS_PER_CELL: f64 = 1.5;
pub const TOKENS_FLOOR: i64 = 12;
pub const TOKENS_EXTRA: i64 = 8;
pub const TOKENS_CEILING: i64 = 160;
pub const SHORT_LINE_GLYPHS: usize = 2;
pub const SHORT_LINE_MIN_CONF: f64 = 0.9;
pub const CTC_DOUBT_CONF: f64 = 0.5;
pub const DETECTOR_SURE: f64 = 0.80;
pub const DETECTOR_REGION: f64 = 0.60;
pub const DETECTOR_BODY: f64 = 0.65;
pub const BODY_MAX_PITCH_SHARE: f64 = 0.8;
pub const BODY_MIN_NEIGHBOURS: usize = 1;
pub const CONFIRMED_CONF: f64 = 0.75;
pub const CONFIRMED_MIN_AGREEMENT: f64 = 0.85;
pub const SKIPPED_MIN_CONF: f64 = 0.94;
pub const KANA_STANDS_CONF: f64 = 0.995;
pub const IDEOGRAPHIC_SPACE: char = '\u{3000}';
pub const DASH: char = '―';
pub const ENGINE_DASHES: &str = "-‐–—―";
pub const LONG_VOWEL: char = 'ー';
pub const THIN_GLYPHS: &str = "一―";
pub const MISSING_GLYPH: char = '〓';
const DOTS: &str = ".…‥";
const BLANKS: &str = "\u{3000} ";
pub const ELLIPSIS: &str = "…";

fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

/// NFKC without any whitespace: what the engines are trained to write.
pub fn fold(text: &str) -> String {
    nfkc(text).chars().filter(|c| !is_space(*c)).collect()
}

fn openers_folded() -> String {
    fold(OPENERS)
}

fn closers_folded() -> String {
    fold(CLOSERS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    value: String,
    start: usize,
    end: usize,
}

/// `text` (as chars) as alignment tokens: NFKC-folded characters, whitespace
/// dropped, a run of dots one token.
fn tokens(text: &[char], blanks: bool) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    for (i, &ch) in text.iter().enumerate() {
        if BLANKS.contains(ch) && blanks {
            out.push(Token { value: ch.to_string(), start: i, end: i + 1 });
            continue;
        }
        let folded: String = std::iter::once(ch).nfkc().filter(|c| !is_space(*c)).collect();
        if !folded.is_empty() && folded.chars().all(|c| DOTS.contains(c)) {
            if let Some(last) = out.last_mut()
                && last.end == i
                && (last.value == "." || last.value == ELLIPSIS)
            {
                *last = Token { value: ELLIPSIS.to_string(), start: last.start, end: i + 1 };
            } else {
                let value = if folded.chars().count() > 1 { ELLIPSIS.to_string() } else { ".".to_string() };
                out.push(Token { value, start: i, end: i + 1 });
            }
            continue;
        }
        out.extend(folded.chars().map(|c| Token { value: c.to_string(), start: i, end: i + 1 }));
    }
    out
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn first(v: &str) -> char {
    v.chars().next().unwrap_or('\0')
}

fn is_japanese(ch: char) -> bool {
    let o = ch as u32;
    (0x3041..=0x30FF).contains(&o) || (0x3400..=0x9FFF).contains(&o) || "々〆〇".contains(ch)
}

fn is_kanji(ch: char) -> bool {
    (0x3400..=0x9FFF).contains(&(ch as u32)) || "々〆〇".contains(ch)
}

fn is_kana(ch: char) -> bool {
    (0x3041..=0x30FF).contains(&(ch as u32))
}

fn is_plain_kana(ch: char) -> bool {
    is_kana(ch) && !"ー・゠".contains(ch)
}

/// Is `s` a printed form NFKC would lose, and one worth giving back?
fn keeps_width(s: &str) -> bool {
    if nfkc(s) == s {
        return false;
    }
    !(0xFF61..=0xFF9F).contains(&(first(s) as u32))
}

/// Full-width forms for marks the engine folded, where no CTC read vouches.
pub fn widen_punctuation(text: &str, keep: Option<&[bool]>) -> String {
    let t = chars(text);
    if !t.iter().any(|c| "!?.".contains(*c)) || !t.iter().any(|c| is_japanese(*c)) {
        return text.to_string();
    }
    let kept: Vec<bool> = keep.map(<[bool]>::to_vec).unwrap_or_else(|| vec![false; t.len()]);
    let mut out = String::new();
    let mut i = 0;
    while i < t.len() {
        let ch = t[i];
        if !"!?.".contains(ch) || kept[i] {
            out.push(ch);
            i += 1;
            continue;
        }
        let mut j = i;
        let dots = ch == '.';
        while j < t.len() && "!?.".contains(t[j]) && !kept[j] && (t[j] == '.') == dots {
            j += 1;
        }
        let run: String = t[i..j].iter().collect();
        let len = j - i;
        let after_ascii = i > 0 && is_ascii_alnum(t[i - 1]);
        let before_ascii = j < t.len() && is_ascii_alnum(t[j]);
        if after_ascii || (ch == '.' && before_ascii) || (ch == '.' && len == 1) {
            out.push_str(&run);
        } else if ch == '.' {
            if len == 2 {
                out.push('‥');
            } else {
                let n = ((len as f64) / 3.0).round_ties_even().max(1.0) as usize;
                out.push_str(&ELLIPSIS.repeat(n));
            }
        } else if len == 1 {
            out.push(if ch == '!' { '！' } else { '？' });
        } else {
            out.push_str(&run);
        }
        i = j;
    }
    out
}

/// Python `int(round(x))`.
fn round_i(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Glyph cells a quad that is NO line has room for: columns x rows at `pitch`.
pub fn region_cells(main: f64, thickness: f64, pitch: f64) -> i64 {
    if main <= 0.0 || thickness <= 0.0 || pitch <= 0.0 {
        return 0;
    }
    round_i(main / pitch).max(1) * round_i(thickness / pitch).max(1)
}

/// Is this quad a region of several columns rather than one line?
pub fn is_region(main: f64, thickness: f64, pitch: f64) -> bool {
    if pitch <= 0.0 || main <= 0.0 || thickness <= 0.0 {
        return false;
    }
    thickness >= REGION_MIN_PITCHES * pitch
}

/// Glyph cells a quad has room for (a region counts columns x rows).
pub fn line_cells(main: f64, thickness: f64, pitch: f64) -> i64 {
    if main <= 0.0 || thickness <= 0.0 {
        return 0;
    }
    let mut cells = round_i(main / thickness).max(1);
    if is_region(main, thickness, pitch) {
        cells = cells.max(region_cells(main, thickness, pitch));
    }
    cells
}

/// Glyph cells along the READING axis.
pub fn axis_cells(main: f64, thickness: f64, pitch: f64) -> i64 {
    if main <= 0.0 || thickness <= 0.0 {
        return 0;
    }
    if is_region(main, thickness, pitch) {
        return round_i(main / pitch).max(1);
    }
    round_i(main / thickness).max(1)
}

/// `max_new_tokens` for a line of `cells` glyph cells.
pub fn token_cap(cells: i64) -> i64 {
    let wanted = (cells.max(0) as f64 * TOKENS_PER_CELL).ceil() as i64 + TOKENS_EXTRA;
    wanted.clamp(TOKENS_FLOOR, TOKENS_CEILING)
}

/// Length of a tail made of one short unit repeated 4+ times, else 0.
pub fn repeated_tail(text: &str) -> usize {
    repeated_tail_chars(&chars(text))
}

fn repeated_tail_chars(t: &[char]) -> usize {
    let n = t.len();
    let mut best = 0;
    for unit in 1..=REPEAT_MAX_UNIT {
        if n < unit * REPEAT_MIN_COUNT {
            break;
        }
        let tail = &t[n - unit..];
        let mut count = 1;
        loop {
            let start = n as i64 - (unit * (count + 1)) as i64;
            let end = n as i64 - (unit * count) as i64;
            let start = start.max(0) as usize;
            let end = end.max(0) as usize;
            let slice = if end > start { &t[start..end] } else { &t[0..0] };
            if slice == tail {
                count += 1;
            } else {
                break;
            }
        }
        if count >= REPEAT_MIN_COUNT {
            best = best.max(unit * count);
        }
    }
    best
}

/// Glyph cells `text` takes: Latin letters and digits are set two to the em.
fn cells_of(text: &[char]) -> f64 {
    py::sum(text.iter().map(|&c| if is_ascii_alnum(c) { 0.5 } else { 1.0 }))
}

/// Did the engine repeat one unit until it had filled the quad twice over?
pub fn engine_looped(vlm: &str, cells: i64, axis: i64) -> bool {
    let text = chars(&fold(vlm));
    let repeat = repeated_tail_chars(&text);
    if cells <= 0 || text.is_empty() || repeat == 0 {
        return false;
    }
    if cells_of(&text) > LOOP_RATIO * cells as f64 + RUNAWAY_SLACK {
        return true;
    }
    if (repeat as f64) < LOOP_MIN_SHARE * text.len() as f64 {
        return false;
    }
    let room = if axis > 0 { axis } else { cells };
    cells_of(&text[text.len() - repeat..]) > LOOP_RATIO * room as f64 + RUNAWAY_SLACK
}

/// Did the engine keep generating past the line?
pub fn is_runaway(text: &str, ctc: &str, cells: i64) -> bool {
    let room = py::max2(cells_of(&chars(&fold(ctc))), cells as f64);
    if room <= 0.0 {
        return false;
    }
    let length = cells_of(&chars(&fold(text)));
    if length > RUNAWAY_RATIO * room + RUNAWAY_SLACK {
        return true;
    }
    length > room + RUNAWAY_SLACK && repeated_tail(text) > 0
}

/// Where a line's text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Merged,
    Ctc,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Merged => "merged",
            Source::Ctc => "ctc",
        }
    }
}

/// One line after the merge.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciled {
    /// The text the sidecar gets.
    pub text: String,
    pub vlm: String,
    pub ctc: String,
    pub agreement: Option<f64>,
    pub source: Source,
    pub notes: Vec<String>,
    pub second: Option<String>,
    pub engine_only: bool,
    pub confirmed: bool,
}

impl Reconciled {
    fn new(text: String, vlm: &str, ctc: &str, agreement: Option<f64>, source: Source, notes: Vec<String>) -> Self {
        Reconciled {
            text,
            vlm: vlm.to_string(),
            ctc: ctc.to_string(),
            agreement,
            source,
            notes,
            second: None,
            engine_only: false,
            confirmed: false,
        }
    }

    /// What the raw dump merges into the line (`Reconciled.to_json`).
    pub fn to_value(&self) -> Value {
        let mut items: Vec<(String, Value)> = vec![("vlm".into(), self.vlm.clone().into())];
        if let Some(second) = &self.second {
            items.push(("vlm_second".into(), second.clone().into()));
        }
        items.push(("ctc".into(), self.ctc.clone().into()));
        items.push(("merged".into(), self.text.clone().into()));
        items.push((
            "agreement".into(),
            self.agreement.map(|a| Value::Float(round_digits(a, 4))).unwrap_or(Value::Null),
        ));
        items.push(("source".into(), self.source.as_str().into()));
        items.push(("notes".into(), Value::Array(self.notes.iter().map(|n| n.clone().into()).collect())));
        if self.engine_only {
            items.push(("engine_only".into(), true.into()));
            items.push(("confirmed".into(), self.confirmed.into()));
        }
        Value::Object(items)
    }
}

fn ratio(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    SequenceMatcher::new(a, b).ratio()
}

fn matches(a: &[String], b: &[String]) -> usize {
    SequenceMatcher::new(a, b).matching_blocks().iter().map(|m| m.size).sum()
}

fn values(tokens: &[Token]) -> Vec<String> {
    tokens.iter().filter(|t| !BLANKS.contains(t.value.as_str())).map(|t| t.value.clone()).collect()
}

/// `values` without the leading openers and trailing closers.
fn core(values: &[String]) -> Vec<String> {
    let (op, cl) = (openers_folded(), closers_folded());
    let mut head = 0;
    while head < values.len() && op.contains(values[head].as_str()) {
        head += 1;
    }
    let mut tail = values.len();
    while tail > head && cl.contains(values[tail - 1].as_str()) {
        tail -= 1;
    }
    values[head..tail].to_vec()
}

fn dashes_folded(values: &[String]) -> Vec<String> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let dash = ENGINE_DASHES.contains(v.as_str())
                || (v.chars().eq([LONG_VOWEL]) && (i == 0 || !is_kana(first(&values[i - 1]))));
            if dash { DASH.to_string() } else { v.clone() }
        })
        .collect()
}

fn token_values(tokens: &[Token]) -> Vec<String> {
    tokens.iter().map(|t| t.value.clone()).collect()
}

fn slice(t: &[char], a: usize, b: usize) -> String {
    let a = a.min(t.len());
    let b = b.min(t.len()).max(a);
    t[a..b].iter().collect()
}

/// Merge the engine's read of a line with the CTC read.
///
/// `ctc` should already be `layout::normalize_text`-ed; `cells` is the quad's
/// glyph room (0 if unknown); `thin` marks a text-body column; `ctc_conf` and
/// `ctc_char_confs` are the CTC confidences (the latter ignored unless it has
/// one entry a character).
pub fn reconcile_line(
    vlm: &str,
    ctc: &str,
    cells: i64,
    thin: bool,
    ctc_conf: Option<f64>,
    ctc_char_confs: Option<&[f64]>,
) -> Reconciled {
    let vlm_text: String = vlm.chars().filter(|c| !is_space(*c)).collect();
    let ctc_text = py::strip(ctc).to_string();
    let ctc_chars = chars(&ctc_text);
    let mut confs: Vec<f64> = ctc_char_confs.map(<[f64]>::to_vec).unwrap_or_default();
    if confs.len() != ctc_chars.len() || ctc_chars.len() != ctc.chars().count() {
        confs.clear();
    }
    if vlm_text.is_empty() {
        let notes = if ctc_text.is_empty() { vec![] } else { vec!["empty".to_string()] };
        return Reconciled::new(ctc_text, vlm, ctc, None, Source::Ctc, notes);
    }
    let (vlm_text, dash_runs) = adopt_dash_runs(&vlm_text, &ctc_text);
    let vt = chars(&vlm_text);
    let mine = tokens(&vt, false);
    let theirs = tokens(&ctc_chars, true);
    let their_values = values(&theirs);
    if their_values.is_empty() {
        let text = widen_punctuation(&vlm_text, None);
        if engine_looped(&vlm_text, cells, 0) {
            let tc = chars(&text);
            let cut = slice(&tc, 0, tc.len() - repeated_tail_chars(&tc));
            // `text[:cells]`, a negative stop counting from the end as in Python.
            let stop = if cells >= 0 { cells as usize } else { (tc.len() as i64 + cells).max(0) as usize };
            let text = if cut.is_empty() { slice(&tc, 0, stop) } else { cut };
            let mut r = Reconciled::new(text, vlm, ctc, None, Source::Merged, vec!["runaway".to_string()]);
            r.engine_only = true;
            return r;
        }
        let mut r = Reconciled::new(text, vlm, ctc, None, Source::Merged, vec![]);
        r.engine_only = true;
        return r;
    }

    let mine_values = token_values(&mine);
    let folded = dashes_folded(&mine_values);
    let same_line = py::max2(ratio(&folded, &core(&their_values)), ratio(&folded, &their_values)) >= PATCH_MIN_AGREEMENT;

    let mut notes: Vec<String> = if dash_runs > 0 { vec!["dash".to_string()] } else { vec![] };
    let mut replace: HashMap<usize, (usize, String)> = HashMap::new();
    let mut insert_before: HashMap<usize, String> = HashMap::new();
    let mut vouched = vec![false; vt.len()];
    let theirs_all = token_values(&theirs);
    let opcodes = SequenceMatcher::new(&mine_values, &theirs_all).opcodes();

    let anchored = |k: i64, forward: bool| -> bool {
        if k < 0 || k as usize >= opcodes.len() {
            return false;
        }
        let op = opcodes[k as usize];
        if op.tag != Tag::Equal {
            return false;
        }
        let rest = if forward { mine.len() - op.i1 } else { op.i2 };
        op.i2 - op.i1 >= ANCHOR_GLYPHS.min(rest)
    };
    let dashlike = |i: usize| -> bool {
        let value = &mine_values[i];
        if ENGINE_DASHES.contains(value.as_str()) {
            return true;
        }
        value.chars().eq([LONG_VOWEL]) && (i == 0 || !is_kana(first(&mine_values[i - 1])))
    };
    let (op_f, cl_f) = (openers_folded(), closers_folded());

    for (k, op) in opcodes.iter().enumerate() {
        let Opcode { tag, i1, i2, j1, j2 } = *op;
        let k = k as i64;
        if tag == Tag::Equal {
            for (a, b) in mine[i1..i2].iter().zip(&theirs[j1..j2]) {
                if let Some(printed) = printed_form(&vt, &ctc_chars, a, b, &mine, &theirs) {
                    replace.insert(a.start, (a.end, printed));
                    notes.push("width".into());
                }
                for v in &mut vouched[a.start..a.end] {
                    *v = true;
                }
            }
            continue;
        }
        if !same_line || j2 == j1 {
            continue;
        }
        let added: Vec<&str> = theirs[j1..j2].iter().map(|t| t.value.as_str()).collect();
        let source = slice(&ctc_chars, theirs[j1].start, theirs[j2 - 1].end);
        let at = if i1 < mine.len() { mine[i1].start } else { vt.len() };
        let end_marks = format!("{}{}", op_f, if thin { THIN_GLYPHS } else { "" });
        if tag == Tag::Insert && i1 == 0 && added.iter().all(|v| end_marks.contains(v)) {
            if anchored(k + 1, true) {
                insert_before.insert(at, source.clone());
                notes.push(if op_f.contains(added[0]) { "opener" } else { "thin" }.into());
            }
            continue;
        }
        let end_marks = format!("{}{}", cl_f, if thin { THIN_GLYPHS } else { "" });
        if tag == Tag::Insert && i1 == mine.len() && added.iter().all(|v| end_marks.contains(v)) {
            if anchored(k - 1, false) {
                insert_before.insert(at, source.clone());
                notes.push(if cl_f.contains(added[added.len() - 1]) { "closer" } else { "thin" }.into());
            }
            continue;
        }
        let s0 = theirs[j1].start.min(confs.len());
        let s1 = theirs[j2 - 1].end.min(confs.len()).max(s0);
        let stretch = &confs[s0..s1];
        if tag == Tag::Insert && sure_text(&source, stretch, SKIPPED_MIN_CONF) {
            let before = i1 == 0 || anchored(k - 1, false);
            let after = i1 == mine.len() || anchored(k + 1, true);
            if before && after {
                insert_before.entry(at).or_default().push_str(&source);
                notes.push("skipped".into());
            }
            continue;
        }
        if tag == Tag::Replace
            && mine_values[i1..i2].iter().all(|v| is_plain_kana(first(v)) && v.chars().count() == 1)
            && source.chars().all(is_plain_kana)
            && !stretch.is_empty()
            && py::min_of(stretch.iter().copied()) >= KANA_STANDS_CONF
        {
            replace.insert(mine[i1].start, (mine[i2 - 1].end, source.clone()));
            notes.push("kana".into());
            continue;
        }
        if source.trim_matches(' ').is_empty() {
            let around: &[String] = if tag == Tag::Insert && 0 < i1 { &mine_values[i1 - 1..(i1 + 1).min(mine_values.len())] } else { &[] };
            if around.len() == 2
                && around.iter().all(|v| v.chars().count() == 1 && is_ascii_alnum(first(v)))
                && (anchored(k - 1, false) || anchored(k + 1, true))
            {
                insert_before.entry(at).or_default().push(' ');
                notes.push("space".into());
            }
            continue;
        }
        if source.chars().any(|c| c != IDEOGRAPHIC_SPACE && c != DASH) {
            continue;
        }
        if !(i1..i2).all(dashlike) {
            continue;
        }
        let beside_dash = [i1 as i64 - 1, i2 as i64]
            .iter()
            .any(|&i| 0 <= i && (i as usize) < mine.len() && dashlike(i as usize));
        let after_mark = 0 < i1 && "!?".contains(mine_values[i1 - 1].as_str());
        let has_dash = source.contains(DASH);
        if i2 > i1 || (has_dash && beside_dash) || (!has_dash && after_mark) {
            if i2 > i1 {
                replace.insert(mine[i1].start, (mine[i2 - 1].end, source.clone()));
            } else {
                insert_before.entry(at).or_default().push_str(&source);
            }
            notes.push(if has_dash { "dash" } else { "space" }.into());
        }
    }

    let merged = assemble(&vt, &replace, &insert_before, &vouched);
    let mut notes = dedupe(notes);
    if !same_line {
        notes.push("disagree".into());
    }
    if is_runaway(&merged, &ctc_text, cells) {
        let agreement = ratio(&mine_values, &their_values);
        notes.push("runaway".into());
        return Reconciled::new(ctc_text, vlm, ctc, Some(agreement), Source::Ctc, notes);
    }
    let their_core = core(&their_values);
    let merged_values = values(&tokens(&chars(&merged), false));
    if let Some(conf) = ctc_conf
        && conf >= SHORT_LINE_MIN_CONF
        && their_core.len() <= SHORT_LINE_GLYPHS
        && core(&merged_values) != their_core
        && (their_core.iter().any(|v| is_kanji(first(v)))
            || (their_core.iter().any(|v| is_japanese(first(v))) && !merged.chars().any(is_japanese))
            || lacks_end_bracket(&merged_values, &their_values))
    {
        let agreement = ratio(&mine_values, &their_values);
        notes.push("short".into());
        return Reconciled::new(ctc_text, vlm, ctc, Some(agreement), Source::Ctc, notes);
    }
    let agreement = ratio(&merged_values, &their_values);
    let doubted = ctc_conf.is_some_and(|c| c < CTC_DOUBT_CONF) && !same_line;
    let mut r = Reconciled::new(merged, vlm, ctc, Some(agreement), Source::Merged, notes);
    r.engine_only = doubted;
    r
}

fn dedupe(notes: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in notes {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Is `source` text, every glyph of it read with confidence `floor` or better?
fn sure_text(source: &str, confs: &[f64], floor: f64) -> bool {
    let src = chars(source);
    if src.is_empty() || confs.len() != src.len() {
        return false;
    }
    let brackets: String = format!("{OPENERS}{CLOSERS}").chars().filter(|c| *c != '。' && *c != '、').collect();
    for (&ch, &conf) in src.iter().zip(confs) {
        if brackets.contains(ch) || BLANKS.contains(ch) {
            continue;
        }
        if !(is_japanese(ch) || "。、！？…‥".contains(ch)) || conf < floor {
            return false;
        }
    }
    src.iter().any(|c| !BLANKS.contains(*c) && !brackets.contains(*c))
}

/// Does the CTC read open or close with a bracket the merged text does not have there?
fn lacks_end_bracket(merged: &[String], theirs: &[String]) -> bool {
    if theirs.is_empty() {
        return false;
    }
    let closers: String = closers_folded().chars().filter(|c| *c != '。' && *c != '、').collect();
    let opens = openers_folded().contains(theirs[0].as_str()) && merged.first() != Some(&theirs[0]);
    let last = &theirs[theirs.len() - 1];
    let closes = closers.contains(last.as_str()) && merged.last() != Some(last);
    opens || closes
}

/// The engine's single dash swapped for the CTC read's RUN of dashes.
fn adopt_dash_runs(vlm_text: &str, ctc_text: &str) -> (String, usize) {
    let mine = tokens(&chars(vlm_text), false);
    let theirs = tokens(&chars(ctc_text), true);
    let mv = token_values(&mine);
    let tv = token_values(&theirs);
    let opcodes = SequenceMatcher::new(&mv, &tv).opcodes();
    let (op_f, cl_f) = (openers_folded(), closers_folded());
    let mut out = chars(vlm_text);
    let mut runs = 0;
    for op in opcodes.iter().rev() {
        let Opcode { tag, i1, i2, mut j1, mut j2 } = *op;
        if tag != Tag::Replace || i2 - i1 != 1 {
            continue;
        }
        if !mine[i1].value.chars().eq([LONG_VOWEL]) && !ENGINE_DASHES.contains(mine[i1].value.as_str()) {
            continue;
        }
        while j2 > j1 && j2 == theirs.len() && cl_f.contains(theirs[j2 - 1].value.as_str()) {
            j2 -= 1;
        }
        while j1 < j2 && j1 == 0 && op_f.contains(theirs[j1].value.as_str()) {
            j1 += 1;
        }
        let run = &tv[j1..j2];
        if run.len() >= 2 && run.iter().all(|v| v.chars().eq([DASH])) {
            let (s, e) = (mine[i1].start.min(out.len()), mine[i1].end.min(out.len()));
            let mut next: Vec<char> = out[..s].to_vec();
            next.extend(std::iter::repeat_n(DASH, run.len()));
            next.extend_from_slice(&out[e..]);
            out = next;
            runs += 1;
        }
    }
    (out.into_iter().collect(), runs)
}

/// The CTC read's printed form of an agreed token, if it differs.
fn printed_form(vlm: &[char], ctc: &[char], mine: &Token, theirs: &Token, all_mine: &[Token], all_theirs: &[Token]) -> Option<String> {
    let engine = slice(vlm, mine.start, mine.end);
    let printed = slice(ctc, theirs.start, theirs.end);
    if engine == printed {
        return None;
    }
    if mine.value == ELLIPSIS {
        if printed.chars().all(|c| c == '…' || c == '‥') {
            return Some(printed);
        }
        let dots = printed.chars().filter(|c| *c == '.').count();
        let cells = printed.chars().filter(|c| *c == '…' || *c == '‥').count() as i64
            + ((dots as f64) / 3.0).round_ties_even() as i64;
        let engine_len = engine.replace("...", ELLIPSIS).chars().count() as i64;
        return if cells > engine_len { Some(ELLIPSIS.repeat(cells.max(0) as usize)) } else { None };
    }
    for (token, tokens) in [(mine, all_mine), (theirs, all_theirs)] {
        if tokens.iter().filter(|t| t.start == token.start).count() != 1 {
            return None;
        }
    }
    if keeps_width(&printed) { Some(printed) } else { None }
}

/// The merged text; unvouched ASCII marks then get the default width policy.
fn assemble(vlm: &[char], replace: &HashMap<usize, (usize, String)>, insert_before: &HashMap<usize, String>, vouched: &[bool]) -> String {
    let mut out: Vec<char> = Vec::new();
    let mut keep: Vec<bool> = Vec::new();
    let mut i = 0;
    while i <= vlm.len() {
        if let Some(extra) = insert_before.get(&i) {
            for c in extra.chars() {
                out.push(c);
                keep.push(true);
            }
        }
        if i == vlm.len() {
            break;
        }
        let (end, new) = match replace.get(&i) {
            Some((end, new)) => (*end, new.clone()),
            None => (i + 1, vlm[i].to_string()),
        };
        for c in new.chars() {
            out.push(c);
            keep.push(vouched[i] || replace.contains_key(&i));
        }
        i = end;
    }
    let text: String = out.into_iter().collect();
    widen_punctuation(&text, Some(&keep))
}

/// Is a second engine read worth its time for this line?
pub fn needs_second_read(line: &Reconciled) -> bool {
    if line.source == Source::Ctc {
        return !line.notes.is_empty() && !line.notes.iter().any(|n| n == "short");
    }
    if line.engine_only {
        return !line.text.is_empty();
    }
    line.agreement.is_some_and(|a| a < 1.0)
}

/// Let a second engine read break the ties the merge left (two of three win).
pub fn settle_disputes(mut line: Reconciled, second: &str, cells: i64) -> Reconciled {
    let second_text: String = second.chars().filter(|c| !is_space(*c)).collect();
    if line.source == Source::Ctc {
        let mut retry = reconcile_line(second, &line.ctc, cells, false, None, None);
        if !second_text.is_empty() && retry.source == Source::Merged && !retry.notes.iter().any(|n| n == "disagree") {
            retry.vlm = line.vlm.clone();
            retry.second = Some(second.to_string());
            let mut notes: Vec<String> =
                line.notes.iter().filter(|n| *n == "empty" || *n == "runaway").cloned().collect();
            notes.extend(retry.notes.iter().cloned());
            notes.push("retry".into());
            retry.notes = notes;
            return retry;
        }
        line.second = Some(second.to_string());
        return line;
    }
    line.second = Some(second.to_string());
    if second_text.is_empty() {
        return line;
    }
    if line.engine_only {
        if corroborates(&line.vlm, &second_text, cells) {
            line.confirmed = true;
            line.notes.push("confirmed".into());
        }
        return line;
    }
    let witness = dashes_folded(&token_values(&tokens(&chars(&second_text), false)));
    let ctc_stripped = chars(py::strip(&line.ctc));
    let theirs = tokens(&ctc_stripped, true);
    let mut text = chars(&line.text);
    let mut changed = false;
    let mine = tokens(&text, false);
    let ops = SequenceMatcher::new(&token_values(&mine), &token_values(&theirs)).opcodes();
    for (i1, i2, j1, j2) in single_disputes(&ops).into_iter().rev() {
        let source = if j2 > j1 { slice(&ctc_stripped, theirs[j1].start, theirs[j2 - 1].end) } else { String::new() };
        if !source.is_empty() && source.chars().all(|c| BLANKS.contains(c)) {
            continue;
        }
        if source.contains(MISSING_GLYPH) {
            continue;
        }
        let start = if i1 < mine.len() { mine[i1].start } else { text.len() };
        let end = if i2 > i1 { mine[i2 - 1].end } else { start };
        let (s, e) = (start.min(text.len()), end.min(text.len()).max(start.min(text.len())));
        let mut swapped: Vec<char> = text[..s].to_vec();
        swapped.extend(source.chars());
        swapped.extend_from_slice(&text[e..]);
        let before = matches(&dashes_folded(&token_values(&tokens(&text, false))), &witness);
        let after = matches(&dashes_folded(&token_values(&tokens(&swapped, false))), &witness);
        if after > before {
            text = swapped;
            changed = true;
        }
    }
    if !changed {
        return line;
    }
    let text: String = text.into_iter().collect();
    let agreement = ratio(&values(&tokens(&chars(&text), false)), &values(&theirs));
    let mut notes = line.notes.clone();
    notes.push("vote".into());
    let mut r = Reconciled::new(text, &line.vlm, &line.ctc, Some(agreement), Source::Merged, notes);
    r.second = Some(second.to_string());
    r
}

/// Does a second engine read say the first read's line again?
pub fn corroborates(vlm: &str, second: &str, cells: i64) -> bool {
    let mine = values(&tokens(&chars(&fold(vlm)), false));
    let theirs = values(&tokens(&chars(&fold(second)), false));
    if mine.is_empty() || theirs.is_empty() {
        return false;
    }
    let shared = matches(&mine, &theirs);
    if cells >= ROOM_MIN_CELLS as i64
        && shared >= ROOM_MIN_CELLS
        && shared as f64 >= CONFIRMED_MIN_AGREEMENT * mine.len().min(theirs.len()) as f64
    {
        return true;
    }
    ratio(&mine, &theirs) >= CONFIRMED_MIN_AGREEMENT
}

/// Keep a line the CTC recognizer never backed, or drop it? `(keep, why)`.
pub fn engine_only_verdict(
    line: &Reconciled,
    cells: i64,
    det_score: f64,
    main: f64,
    thickness: f64,
    pitch: f64,
    neighbours: usize,
) -> (bool, &'static str) {
    if engine_looped(&line.vlm, cells, axis_cells(main, thickness, pitch)) {
        return (false, "looped");
    }
    if det_score >= DETECTOR_SURE {
        return (true, "backed");
    }
    if det_score >= DETECTOR_REGION && is_region(main, thickness, pitch) && fold(&line.text).chars().count() >= ROOM_MIN_CELLS {
        return (true, "region");
    }
    if det_score >= DETECTOR_BODY
        && pitch > 0.0
        && neighbours >= BODY_MIN_NEIGHBOURS
        && 0.0 < thickness
        && thickness <= BODY_MAX_PITCH_SHARE * pitch
        && !fold(&line.text).is_empty()
    {
        return (true, "body");
    }
    (false, "unbacked")
}

/// The differing stretches of an alignment, cut down to one glyph each.
fn single_disputes(opcodes: &[Opcode]) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    for op in opcodes {
        if op.tag == Tag::Equal {
            continue;
        }
        let (i1, i2, j1, j2) = (op.i1, op.i2, op.j1, op.j2);
        let pairs = (i2 - i1).min(j2 - j1);
        if i1 == 0 && pairs > 0 {
            out.push((i1, i2 - pairs, j1, j2 - pairs));
            for n in (1..=pairs).rev() {
                out.push((i2 - n, i2 - n + 1, j2 - n, j2 - n + 1));
            }
        } else {
            for n in 0..pairs {
                out.push((i1 + n, i1 + n + 1, j1 + n, j1 + n + 1));
            }
            out.push((i1 + pairs, i2, j1 + pairs, j2));
        }
    }
    out.into_iter().filter(|&(i1, i2, j1, j2)| i2 > i1 || j2 > j1).collect()
}

/// Glyphs at the start of `after` that repeat the end of `before`.
pub fn overlap_repeat(before: &str, after: &str, max_glyphs: i64) -> usize {
    let a = chars(py::strip(before));
    let b = chars(py::strip(after));
    let top = a.len().min(b.len()).min(max_glyphs.max(0) as usize);
    for n in (1..=top).rev() {
        if a[a.len() - n..] == b[..n] {
            return n;
        }
    }
    0
}

/// Tally for the raw dump and the log: how the page's lines were settled.
pub fn page_summary(lines: &[Reconciled]) -> Value {
    let compared: Vec<f64> = lines.iter().filter_map(|l| l.agreement).collect();
    let mut notes: Vec<(String, i64)> = Vec::new();
    for l in lines {
        for n in &l.notes {
            if let Some(slot) = notes.iter_mut().find(|(k, _)| k == n) {
                slot.1 += 1;
            } else {
                notes.push((n.clone(), 1));
            }
        }
    }
    notes.sort_by(|a, b| a.0.cmp(&b.0));
    let mean = if compared.is_empty() {
        Value::Null
    } else {
        Value::Float(round_digits(py::sum(compared.iter().copied()) / compared.len() as f64, 4))
    };
    Value::Object(vec![
        ("lines".into(), Value::Int(lines.len() as i64)),
        ("compared".into(), Value::Int(compared.len() as i64)),
        ("full_agreement".into(), Value::Int(compared.iter().filter(|a| **a >= 1.0).count() as i64)),
        ("mean_agreement".into(), mean),
        ("from_ctc".into(), Value::Int(lines.iter().filter(|l| l.source == Source::Ctc).count() as i64)),
        ("notes".into(), Value::Object(notes.into_iter().map(|(k, v)| (k, Value::Int(v))).collect())),
    ])
}
