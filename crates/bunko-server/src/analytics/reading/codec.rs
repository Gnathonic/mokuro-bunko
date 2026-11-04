//! Mokuro Reader's reading-history segments (`history/<device>/<YYYY-MM>.events`,
//! `legacy.events`): a port of `decodeSegment` in the reader's
//! `src/lib/reading-history/segment-codec.ts` (develop 085453c2), format 1.
//!
//! A segment is raw-deflated JSON: a header (`format`, `device`, `month`, `first_seq`,
//! `last_seq`, `count`, `legacy`), a `volumes` dictionary, an optional `targets`
//! dictionary and `rows`, one compact row per event with `seq` and `t` delta-encoded:
//!
//! ```text
//! 0 page     [0, dSeq, dT, vol, first, last, page_chars[], chars_before, dwell|-1, layout, orientation, w|-1, h|-1]
//! 1 adjust   [1, dSeq, dT, vol, time_delta_ms, chars_delta]
//! 2 restart  [2, dSeq, dT, vol]
//! 3 forget   [3, dSeq, dT, vol, before]
//! 4 position [4, dSeq, dT, vol, answer (0 jump, 1 stay, 2 reset), through, page]
//! 5 resolve  [5, dSeq, dT, vol, target device, target seq, count (0 full, 1 typical, 2 none)]
//! ```
//!
//! The port accepts and refuses what the reader does, cell by cell (the same `num` /
//! `int` / `numbers` checks, JS strict equality for the kind and `-1`, the dictionary
//! lookups, the per-device `seq` order and the header's seq range), including what
//! `JSON.parse` and `DecompressionStream` do: trailing bytes after the deflate stream
//! are refused, a leading byte-order mark and invalid UTF-8 are read as
//! `Response.text()` reads them, a later duplicate key wins, the keys may come in any
//! order. Not ported: JavaScript's property-key coercion of a non-number,
//! non-string cell used as an index (an array `[0]` naming volume 0), and a number
//! too large for a double in a cell nobody reads (`1e400`, which `JSON.parse` turns into
//! `Infinity`); the reader's encoder writes neither.
//!
//! Bounded: at most [`MAX_INFLATED`] bytes of JSON (a small upload can inflate to a lot)
//! and [`MAX_EVENTS`] rows, so one file costs a few tens of MiB at worst.

use serde::Deserializer as _;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;

/// The only segment format there is (`HISTORY_SEGMENT_FORMAT`).
pub const FORMAT: f64 = 1.0;
/// Most bytes a segment may inflate to.
pub const MAX_INFLATED: usize = 16 << 20;
/// Most rows one segment may hold.
pub const MAX_EVENTS: usize = 250_000;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
/// Cells a row is read for (a page row's 13; more are ignored, as in the reader).
const CELLS: usize = 13;

/// Why a segment was not read. Every one is the reader's `SegmentFormatError` too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
    /// Not raw deflate, or not JSON (`unreadable segment`).
    Unreadable(String),
    /// More than [`MAX_INFLATED`] bytes, or [`MAX_EVENTS`] rows (this port's bound).
    TooLarge,
    /// `format` is not 1 (a newer reader's file).
    UnsupportedFormat,
    /// Decodable, but a header field or a cell is not what format 1 says.
    Malformed(&'static str),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentError::Unreadable(e) => write!(f, "unreadable segment: {e}"),
            SegmentError::TooLarge => write!(f, "segment too large"),
            SegmentError::UnsupportedFormat => write!(f, "unsupported segment format"),
            SegmentError::Malformed(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Single,
    Double,
    ContinuousV,
    ContinuousH,
    Unknown,
}

const LAYOUTS: [Layout; 5] = [
    Layout::Single,
    Layout::Double,
    Layout::ContinuousV,
    Layout::ContinuousH,
    Layout::Unknown,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    Landscape,
    Unknown,
}

const ORIENTATIONS: [Orientation; 3] = [
    Orientation::Portrait,
    Orientation::Landscape,
    Orientation::Unknown,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Jump,
    Stay,
    Reset,
}

const ANSWERS: [Answer; 3] = [Answer::Jump, Answer::Stay, Answer::Reset];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseCount {
    Full,
    Typical,
    None,
}

const COUNTS: [PauseCount; 3] = [PauseCount::Full, PauseCount::Typical, PauseCount::None];

/// The segment header (`SegmentHeader`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub device: String,
    pub month: String,
    pub first_seq: i64,
    pub last_seq: i64,
    pub count: usize,
    pub legacy: bool,
}

/// What an event says (`EventPayload`), dictionary entries as indices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Payload {
    Page {
        first_page: i64,
        last_page: i64,
        /// `sum(page_chars)` and its length (the values: [`Segment::page_chars`]).
        chars_sum: f64,
        chars_len: u32,
        chars_at: u32,
        chars_before: f64,
        dwell_ms: Option<f64>,
        layout: Layout,
        orientation: Orientation,
        viewport: Option<(f64, f64)>,
    },
    Adjust {
        time_delta_ms: f64,
        chars_delta: f64,
    },
    Restart,
    Forget {
        before: f64,
    },
    Position {
        answer: Answer,
        through: f64,
        page: i64,
    },
    Resolve {
        /// Index into [`Segment::targets`].
        target: usize,
        target_seq: i64,
        count: PauseCount,
    },
}

/// One event (`ReadingEvent`); its device is [`Segment::device_of`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    pub seq: i64,
    pub t: i64,
    /// Index into [`Segment::volumes`] (always a string there).
    pub volume: usize,
    pub payload: Payload,
}

/// A decoded segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub header: Header,
    /// The volume dictionary; entries nobody references may be anything (`None`).
    pub volumes: Vec<Option<String>>,
    pub targets: Vec<String>,
    pub events: Vec<Event>,
    /// Every page event's `page_chars`, when decoded with `keep_page_chars`.
    page_chars: Vec<f64>,
}

impl Segment {
    pub fn volume(&self, e: &Event) -> &str {
        self.volumes
            .get(e.volume)
            .and_then(|v| v.as_deref())
            .unwrap_or_default()
    }

    /// The event's device: the header's, or `legacy:<volume>` in a legacy segment.
    pub fn device_of(&self, e: &Event) -> String {
        if self.header.legacy {
            format!("legacy:{}", self.volume(e))
        } else {
            self.header.device.clone()
        }
    }

    /// A page event's `page_chars` (empty unless decoded with `keep_page_chars`).
    pub fn page_chars(&self, e: &Event) -> &[f64] {
        match e.payload {
            Payload::Page {
                chars_at,
                chars_len,
                ..
            } => self
                .page_chars
                .get(chars_at as usize..(chars_at as usize + chars_len as usize))
                .unwrap_or_default(),
            _ => &[],
        }
    }
}

/// How to decode.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Keep every `page_chars` value (the sums are always kept).
    pub keep_page_chars: bool,
    pub max_events: usize,
    pub max_inflated: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep_page_chars: false,
            max_events: MAX_EVENTS,
            max_inflated: MAX_INFLATED,
        }
    }
}

/// `decodeSegment(bytes)` with the default bounds.
pub fn decode(bytes: &[u8]) -> Result<Segment, SegmentError> {
    decode_with(bytes, Options::default())
}

pub fn decode_with(bytes: &[u8], opts: Options) -> Result<Segment, SegmentError> {
    let inflated = inflate_raw(bytes, opts.max_inflated)?;
    // `Response.text()`: UTF-8 with replacement, a leading BOM dropped.
    let text = match String::from_utf8(inflated) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut parsed = Parsed {
        keep_page_chars: opts.keep_page_chars,
        max_events: opts.max_events,
        ..Parsed::default()
    };
    let mut de = serde_json::Deserializer::from_str(text);
    (&mut de)
        .deserialize_any(TopVisitor(&mut parsed))
        .map_err(|e| SegmentError::Unreadable(e.to_string()))?;
    de.end()
        .map_err(|e| SegmentError::Unreadable(e.to_string()))?;
    parsed.finish()
}

/// Raw deflate (`DecompressionStream('deflate-raw')`): the whole input must be one
/// stream, ending exactly at the input's end.
fn inflate_raw(input: &[u8], max: usize) -> Result<Vec<u8>, SegmentError> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut z = Decompress::new(false);
    let limit = max.saturating_add(1);
    let mut out: Vec<u8> = Vec::with_capacity(input.len().saturating_mul(4).clamp(1024, limit));
    loop {
        if out.len() >= limit {
            return Err(SegmentError::TooLarge);
        }
        if out.len() == out.capacity() {
            let more = out.len().max(4096).min(limit - out.len());
            out.reserve_exact(more);
        }
        let (in_before, out_before) = (z.total_in(), z.total_out());
        let consumed = usize::try_from(in_before).unwrap_or(usize::MAX);
        let rest = input.get(consumed..).unwrap_or_default();
        let status = z
            .decompress_vec(rest, &mut out, FlushDecompress::None)
            .map_err(|e| SegmentError::Unreadable(e.to_string()))?;
        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                let stuck = z.total_in() == in_before && z.total_out() == out_before;
                if stuck && (rest.is_empty() || out.len() < out.capacity()) {
                    return Err(SegmentError::Unreadable("truncated deflate stream".into()));
                }
            }
        }
    }
    if out.len() > max {
        return Err(SegmentError::TooLarge);
    }
    if usize::try_from(z.total_in()).ok() != Some(input.len()) {
        return Err(SegmentError::Unreadable(
            "trailing bytes after the end of the compressed stream".into(),
        ));
    }
    Ok(out)
}

/// A JSON value where the codec only asks what kind of value it is.
#[derive(Debug, Clone, Default, PartialEq)]
enum Scalar {
    #[default]
    Absent,
    Num(f64),
    Str(String),
    Bool(bool),
    /// null, an array or an object.
    Other,
}

/// A JSON array of scalars (the dictionaries), or something else.
#[derive(Debug, Clone, Default, PartialEq)]
enum List {
    #[default]
    Absent,
    Items(Vec<Scalar>),
    NotArray,
}

/// One cell of a row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Cell {
    #[default]
    Missing,
    Num(f64),
    /// A string: its value as an array index when it is one (`"0"`, `"12"`).
    Str(Option<usize>),
    /// An array: whether every element is a finite number, their sum and count, and
    /// where its values start in the kept `page_chars`.
    List {
        numbers: bool,
        sum: f64,
        len: u32,
        at: u32,
    },
    /// null, a boolean or an object.
    Other,
}

/// `num(value)`: a finite number.
fn num(c: Cell) -> Result<f64, &'static str> {
    match c {
        Cell::Num(v) if v.is_finite() => Ok(v),
        _ => Err("malformed number cell"),
    }
}

/// `int(value)`: `Number.isSafeInteger`.
fn int(c: Cell) -> Result<i64, &'static str> {
    match c {
        Cell::Num(v) if v.fract() == 0.0 && v.abs() <= MAX_SAFE => Ok(v as i64),
        _ => Err("malformed integer cell"),
    }
}

fn safe_int(s: &Scalar) -> Option<i64> {
    match s {
        Scalar::Num(v) if v.fract() == 0.0 && v.abs() <= MAX_SAFE => Some(*v as i64),
        _ => None,
    }
}

/// `value === -1`.
fn is_minus_one(c: Cell) -> bool {
    matches!(c, Cell::Num(v) if v == -1.0)
}

/// `array[value]`'s index: a non-negative integral number, or a string that is an
/// array index (JavaScript converts the key to a string).
fn index_of(c: Cell) -> Option<usize> {
    match c {
        Cell::Num(v) if v >= 0.0 && v.fract() == 0.0 && v < 4_294_967_295.0 => Some(v as usize),
        Cell::Str(i) => i,
        _ => None,
    }
}

/// A canonical array index string (`"0"`, `"7"`, not `"07"` or `"+1"`).
fn array_index(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let canonical = !b.is_empty()
        && b.iter().all(u8::is_ascii_digit)
        && (b.len() == 1 || b[0] != b'0')
        && b.len() <= 10;
    canonical
        .then(|| s.parse::<u64>().ok())
        .flatten()
        .filter(|v| *v < 4_294_967_295)
        .map(|v| v as usize)
}

/// A row, before the dictionaries and the per-device sequence are applied.
#[derive(Debug, Clone, Copy)]
struct PreEvent {
    d_seq: i64,
    d_t: i64,
    volume: Option<usize>,
    payload: PrePayload,
}

#[derive(Debug, Clone, Copy)]
enum PrePayload {
    Ready(Payload),
    /// `targets[target]` is looked up once the dictionary is known.
    Resolve {
        target: i64,
        target_seq: i64,
        count: PauseCount,
    },
}

/// The checks of `decodeSegment`'s row loop that need no header.
fn pre_event(c: &[Cell; CELLS]) -> Result<PreEvent, &'static str> {
    let volume = index_of(c[3]);
    let d_seq = int(c[1])?;
    if d_seq <= 0 {
        return Err("seq must strictly increase");
    }
    let d_t = int(c[2])?;
    let kind = match c[0] {
        Cell::Num(v) if (0.0..=5.0).contains(&v) && v.fract() == 0.0 => v as u8,
        _ => return Err("unknown event kind"),
    };
    let lookup = |cell: Cell, len: usize| index_of(cell).filter(|i| *i < len);
    let payload = match kind {
        0 => {
            let first_page = int(c[4])?;
            let last_page = int(c[5])?;
            let (sum, len, at) = match c[6] {
                Cell::List {
                    numbers: true,
                    sum,
                    len,
                    at,
                } => (sum, len, at),
                _ => return Err("malformed list cell"),
            };
            let chars_before = num(c[7])?;
            let dwell_ms = if is_minus_one(c[8]) {
                None
            } else {
                Some(num(c[8])?)
            };
            let viewport = if is_minus_one(c[11]) {
                None
            } else {
                Some((num(c[11])?, num(c[12])?))
            };
            PrePayload::Ready(Payload::Page {
                first_page,
                last_page,
                chars_sum: sum,
                chars_len: len,
                chars_at: at,
                chars_before,
                dwell_ms,
                layout: lookup(c[9], LAYOUTS.len()).map_or(Layout::Unknown, |i| LAYOUTS[i]),
                orientation: lookup(c[10], ORIENTATIONS.len())
                    .map_or(Orientation::Unknown, |i| ORIENTATIONS[i]),
                viewport,
            })
        }
        1 => PrePayload::Ready(Payload::Adjust {
            time_delta_ms: num(c[4])?,
            chars_delta: num(c[5])?,
        }),
        2 => PrePayload::Ready(Payload::Restart),
        3 => PrePayload::Ready(Payload::Forget { before: num(c[4])? }),
        4 => {
            let answer = usize::try_from(int(c[4])?)
                .ok()
                .and_then(|i| ANSWERS.get(i).copied())
                .ok_or("unknown position answer")?;
            PrePayload::Ready(Payload::Position {
                answer,
                through: num(c[5])?,
                page: int(c[6])?,
            })
        }
        _ => {
            let target = int(c[4])?;
            let target_seq = int(c[5])?;
            if target_seq <= 0 {
                return Err("target seq must be positive");
            }
            let count = usize::try_from(int(c[6])?)
                .ok()
                .and_then(|i| COUNTS.get(i).copied())
                .ok_or("unknown pause answer")?;
            PrePayload::Resolve {
                target,
                target_seq,
                count,
            }
        }
    };
    Ok(PreEvent {
        d_seq,
        d_t,
        volume,
        payload,
    })
}

/// What the JSON pass collected.
#[derive(Default)]
struct Parsed {
    keep_page_chars: bool,
    max_events: usize,
    /// The top level was not an object: `Some(true)` for an array.
    not_object: Option<bool>,
    format: Scalar,
    device: Scalar,
    month: Scalar,
    first_seq: Scalar,
    last_seq: Scalar,
    count: Scalar,
    legacy: Scalar,
    volumes: List,
    targets: List,
    rows_seen: bool,
    rows_is_array: bool,
    rows_len: usize,
    rows: Vec<PreEvent>,
    row_error: Option<&'static str>,
    too_many: bool,
    page_chars: Vec<f64>,
}

impl Parsed {
    /// `decodeSegment` after `JSON.parse`, in its order of checks.
    fn finish(self) -> Result<Segment, SegmentError> {
        use SegmentError::Malformed;
        match self.not_object {
            Some(true) => return Err(SegmentError::UnsupportedFormat),
            Some(false) => return Err(Malformed("not a segment")),
            None => {}
        }
        if self.format != Scalar::Num(FORMAT) {
            return Err(SegmentError::UnsupportedFormat);
        }
        if self.too_many {
            return Err(SegmentError::TooLarge);
        }
        let header_error = Malformed("malformed segment header");
        let (Scalar::Str(device), Scalar::Str(month)) = (self.device, self.month) else {
            return Err(header_error);
        };
        let List::Items(volumes) = self.volumes else {
            return Err(header_error);
        };
        if !self.rows_seen || !self.rows_is_array || self.count != Scalar::Num(self.rows_len as f64)
        {
            return Err(header_error);
        }
        let (Some(first_seq), Some(last_seq)) =
            (safe_int(&self.first_seq), safe_int(&self.last_seq))
        else {
            return Err(header_error);
        };
        let targets: Vec<String> = match self.targets {
            List::Absent => Vec::new(),
            List::NotArray => return Err(header_error),
            List::Items(items) => items
                .into_iter()
                .map(|t| match t {
                    Scalar::Str(s) => Ok(s),
                    _ => Err(Malformed("malformed segment header")),
                })
                .collect::<Result<_, _>>()?,
        };
        let legacy = self.legacy == Scalar::Bool(true);
        if let Some(e) = self.row_error {
            return Err(Malformed(e));
        }
        let volumes: Vec<Option<String>> = volumes
            .into_iter()
            .map(|v| match v {
                Scalar::Str(s) => Some(s),
                _ => None,
            })
            .collect();
        let mut events = Vec::with_capacity(self.rows.len());
        let (mut seq, mut t) = (0i64, 0i64);
        let mut row_device: Option<&str> = None;
        let mut finished: HashSet<&str> = HashSet::new();
        for row in &self.rows {
            let index = row.volume.ok_or(Malformed("unknown volume index"))?;
            let volume = volumes
                .get(index)
                .and_then(|v| v.as_deref())
                .ok_or(Malformed("unknown volume index"))?;
            // Seqs restart per device; each device's rows are contiguous. A native
            // segment has one device; a legacy one has one per volume.
            let event_device = if legacy { volume } else { device.as_str() };
            if row_device != Some(event_device) {
                if finished.contains(event_device) {
                    return Err(Malformed("device rows are not contiguous"));
                }
                if let Some(d) = row_device {
                    finished.insert(d);
                }
                row_device = Some(event_device);
                seq = 0;
            }
            seq = seq
                .checked_add(row.d_seq)
                .ok_or(Malformed("seq out of range"))?;
            t = t.checked_add(row.d_t).ok_or(Malformed("t out of range"))?;
            let payload = match row.payload {
                PrePayload::Ready(p) => p,
                PrePayload::Resolve {
                    target,
                    target_seq,
                    count,
                } => {
                    if legacy {
                        return Err(Malformed("resolve in a legacy segment"));
                    }
                    let target = usize::try_from(target)
                        .ok()
                        .filter(|i| targets.get(*i).is_some_and(|d| !d.is_empty()))
                        .ok_or(Malformed("unknown target device"))?;
                    Payload::Resolve {
                        target,
                        target_seq,
                        count,
                    }
                }
            };
            events.push(Event {
                seq,
                t,
                volume: index,
                payload,
            });
        }
        if !legacy
            && let (Some(first), Some(last)) = (events.first(), events.last())
            && (first.seq != first_seq || last.seq != last_seq)
        {
            return Err(Malformed("seq range does not match header"));
        }
        Ok(Segment {
            header: Header {
                device,
                month,
                first_seq,
                last_seq,
                count: self.rows_len,
                legacy,
            },
            volumes,
            targets,
            events,
            page_chars: self.page_chars,
        })
    }
}

struct TopVisitor<'a>(&'a mut Parsed);

impl<'de> Visitor<'de> for TopVisitor<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let p = self.0;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "format" => p.format = map.next_value_seed(ScalarSeed)?,
                "device" => p.device = map.next_value_seed(ScalarSeed)?,
                "month" => p.month = map.next_value_seed(ScalarSeed)?,
                "first_seq" => p.first_seq = map.next_value_seed(ScalarSeed)?,
                "last_seq" => p.last_seq = map.next_value_seed(ScalarSeed)?,
                "count" => p.count = map.next_value_seed(ScalarSeed)?,
                "legacy" => p.legacy = map.next_value_seed(ScalarSeed)?,
                "volumes" => p.volumes = map.next_value_seed(ListSeed)?,
                "targets" => p.targets = map.next_value_seed(ListSeed)?,
                "rows" => {
                    // A later "rows" wins, as JSON.parse keeps the last duplicate.
                    p.rows.clear();
                    p.page_chars.clear();
                    p.row_error = None;
                    p.too_many = false;
                    p.rows_len = 0;
                    p.rows_seen = true;
                    p.rows_is_array = map.next_value_seed(RowsSeed(p))?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        self.0.not_object = Some(true);
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.0.not_object = Some(false);
        Ok(())
    }
}

struct ScalarSeed;

impl<'de> DeserializeSeed<'de> for ScalarSeed {
    type Value = Scalar;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Scalar, D::Error> {
        d.deserialize_any(ScalarVisitor)
    }
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Scalar, E> {
        Ok(Scalar::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Scalar, E> {
        Ok(Scalar::Num(v as f64))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Scalar, E> {
        Ok(Scalar::Num(v as f64))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Scalar, E> {
        Ok(Scalar::Num(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Scalar, E> {
        Ok(Scalar::Str(v.to_string()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Scalar, E> {
        Ok(Scalar::Str(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Scalar, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Scalar::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Scalar, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Scalar::Other)
    }
}

struct ListSeed;

impl<'de> DeserializeSeed<'de> for ListSeed {
    type Value = List;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<List, D::Error> {
        d.deserialize_any(ListVisitor)
    }
}

struct ListVisitor;

impl<'de> Visitor<'de> for ListVisitor {
    type Value = List;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<List, A::Error> {
        let mut items = Vec::new();
        while let Some(s) = seq.next_element_seed(ScalarSeed)? {
            items.push(s);
        }
        Ok(List::Items(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<List, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(List::NotArray)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<List, E> {
        Ok(List::NotArray)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<List, E> {
        Ok(List::NotArray)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<List, E> {
        Ok(List::NotArray)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<List, E> {
        Ok(List::NotArray)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<List, E> {
        Ok(List::NotArray)
    }
    fn visit_unit<E: de::Error>(self) -> Result<List, E> {
        Ok(List::NotArray)
    }
}

/// `rows`: `true` when it is an array. Each row is checked as it is read; after the
/// first bad row (or past the bound) the rest are only skipped.
struct RowsSeed<'a>(&'a mut Parsed);

impl<'de> DeserializeSeed<'de> for RowsSeed<'_> {
    type Value = bool;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RowsSeed<'_> {
    type Value = bool;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let p = self.0;
        loop {
            if p.row_error.is_some() || p.too_many {
                if seq.next_element::<IgnoredAny>()?.is_none() {
                    break;
                }
                p.rows_len += 1;
                continue;
            }
            let mut cells = [Cell::Missing; CELLS];
            let seed = RowSeed {
                cells: &mut cells,
                page_chars: &mut p.page_chars,
                keep: p.keep_page_chars,
            };
            let Some(is_array) = seq.next_element_seed(seed)? else {
                break;
            };
            p.rows_len += 1;
            if !is_array {
                p.row_error = Some("malformed row");
                continue;
            }
            match pre_event(&cells) {
                Ok(e) if p.rows.len() < p.max_events => p.rows.push(e),
                Ok(_) => {
                    p.too_many = true;
                    p.rows.clear();
                    p.page_chars.clear();
                }
                Err(e) => p.row_error = Some(e),
            }
        }
        Ok(true)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(false)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }
}

/// One row: `true` when it is an array (its cells in `cells`).
struct RowSeed<'a> {
    cells: &'a mut [Cell; CELLS],
    page_chars: &'a mut Vec<f64>,
    keep: bool,
}

impl<'de> DeserializeSeed<'de> for RowSeed<'_> {
    type Value = bool;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RowSeed<'_> {
    type Value = bool;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut i = 0usize;
        loop {
            if i >= CELLS {
                if seq.next_element::<IgnoredAny>()?.is_none() {
                    break;
                }
                continue;
            }
            // Only a page row's cell 6 is a list worth keeping.
            let list = i == 6 && self.cells[0] == Cell::Num(0.0);
            let seed = CellSeed {
                page_chars: &mut *self.page_chars,
                keep: self.keep && list,
            };
            match seq.next_element_seed(seed)? {
                Some(c) => self.cells[i] = c,
                None => break,
            }
            i += 1;
        }
        Ok(true)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(false)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }
}

struct CellSeed<'a> {
    page_chars: &'a mut Vec<f64>,
    keep: bool,
}

impl<'de> DeserializeSeed<'de> for CellSeed<'_> {
    type Value = Cell;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Cell, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for CellSeed<'_> {
    type Value = Cell;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Cell, E> {
        Ok(Cell::Other)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Cell, E> {
        Ok(Cell::Num(v as f64))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Cell, E> {
        Ok(Cell::Num(v as f64))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Cell, E> {
        Ok(Cell::Num(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Cell, E> {
        Ok(Cell::Str(array_index(v)))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Cell, E> {
        Ok(Cell::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Cell, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Cell::Other)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Cell, A::Error> {
        let at = self.page_chars.len();
        let (mut numbers, mut sum, mut len) = (true, 0.0f64, 0u32);
        while let Some(c) = seq.next_element_seed(ScalarSeed)? {
            match c {
                Scalar::Num(v) if numbers && v.is_finite() => {
                    sum += v;
                    len = len.saturating_add(1);
                    if self.keep {
                        self.page_chars.push(v);
                    }
                }
                _ => numbers = false,
            }
        }
        if !numbers && self.keep {
            self.page_chars.truncate(at);
        }
        Ok(Cell::List {
            numbers,
            sum,
            len,
            at: u32::try_from(at).unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;

    /// Fixtures made by the reader's own `encodeSegment` (develop 085453c2), with its
    /// own `decodeSegment` output beside each: see `testdata/README.md`.
    const NATIVE: &[u8] = include_bytes!("testdata/device-a-2026-10.events");
    const NATIVE_JSON: &str = include_str!("testdata/device-a-2026-10.json");
    const LEGACY: &[u8] = include_bytes!("testdata/device-a-legacy.events");
    const LEGACY_JSON: &str = include_str!("testdata/device-a-legacy.json");
    const EMPTY: &[u8] = include_bytes!("testdata/device-a-2026-11-empty.events");
    const EMPTY_JSON: &str = include_str!("testdata/device-a-2026-11-empty.json");
    const FORMAT_2: &[u8] = include_bytes!("testdata/format-2.events");

    fn keep() -> Options {
        Options {
            keep_page_chars: true,
            ..Options::default()
        }
    }

    /// The reader's JSON shape of a decoded segment (`{header, events}`).
    fn reader_json(s: &Segment) -> Value {
        let mut header = json!({
            "format": 1,
            "device": s.header.device,
            "month": s.header.month,
            "first_seq": s.header.first_seq,
            "last_seq": s.header.last_seq,
            "count": s.header.count,
        });
        if s.header.legacy {
            header["legacy"] = json!(true);
        }
        let n = |v: f64| {
            if v.fract() == 0.0 && v.abs() <= MAX_SAFE {
                json!(v as i64)
            } else {
                json!(v)
            }
        };
        let events: Vec<Value> = s
            .events
            .iter()
            .map(|e| {
                let mut v = json!({"device": s.device_of(e), "seq": e.seq, "t": e.t});
                let volume = s.volume(e).to_string();
                let body = match e.payload {
                    Payload::Page {
                        first_page,
                        last_page,
                        chars_before,
                        dwell_ms,
                        layout,
                        orientation,
                        viewport,
                        ..
                    } => json!({
                        "kind": "page", "volume": volume, "first_page": first_page,
                        "last_page": last_page,
                        "page_chars": s.page_chars(e).iter().map(|v| n(*v)).collect::<Vec<_>>(),
                        "chars_before": n(chars_before), "dwell_ms": dwell_ms.map(n),
                        "layout": match layout {
                            Layout::Single => "single", Layout::Double => "double",
                            Layout::ContinuousV => "continuous-v",
                            Layout::ContinuousH => "continuous-h", Layout::Unknown => "unknown",
                        },
                        "orientation": match orientation {
                            Orientation::Portrait => "portrait",
                            Orientation::Landscape => "landscape",
                            Orientation::Unknown => "unknown",
                        },
                        "viewport": viewport.map(|(w, h)| json!({"w": n(w), "h": n(h)})),
                    }),
                    Payload::Adjust {
                        time_delta_ms,
                        chars_delta,
                    } => json!({"kind": "adjust", "volume": volume,
                        "time_delta_ms": n(time_delta_ms), "chars_delta": n(chars_delta)}),
                    Payload::Restart => json!({"kind": "restart", "volume": volume}),
                    Payload::Forget { before } => {
                        json!({"kind": "forget", "volume": volume, "before": n(before)})
                    }
                    Payload::Position {
                        answer,
                        through,
                        page,
                    } => json!({"kind": "position", "volume": volume,
                        "answer": match answer { Answer::Jump => "jump", Answer::Stay => "stay", Answer::Reset => "reset" },
                        "through": n(through), "page": page}),
                    Payload::Resolve {
                        target,
                        target_seq,
                        count,
                    } => json!({"kind": "resolve", "volume": volume,
                        "target": [s.targets[target], target_seq],
                        "count": match count { PauseCount::Full => "full", PauseCount::Typical => "typical", PauseCount::None => "none" }}),
                };
                if let (Value::Object(m), Value::Object(b)) = (&mut v, body) {
                    m.extend(b);
                }
                v
            })
            .collect();
        json!({"header": header, "events": events})
    }

    fn golden(bytes: &[u8], expected: &str) {
        let s = decode_with(bytes, keep()).unwrap();
        let ours = reader_json(&s);
        let theirs: Value = serde_json::from_str(expected).unwrap();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn decodes_the_readers_own_segments_exactly() {
        golden(NATIVE, NATIVE_JSON);
        golden(LEGACY, LEGACY_JSON);
        golden(EMPTY, EMPTY_JSON);
        let s = decode(NATIVE).unwrap();
        assert_eq!(s.events.len(), 10);
        assert!(s.page_chars.is_empty(), "values are not kept by default");
        match s.events[0].payload {
            Payload::Page {
                chars_sum,
                chars_len,
                ..
            } => assert_eq!((chars_sum, chars_len), (120.0, 2)),
            _ => panic!("a page event"),
        }
    }

    #[test]
    fn refuses_what_the_reader_refuses() {
        assert_eq!(decode(FORMAT_2), Err(SegmentError::UnsupportedFormat));
        assert!(matches!(
            decode(&[1, 2, 3, 4]),
            Err(SegmentError::Unreadable(_))
        ));
        assert!(matches!(decode(&[]), Err(SegmentError::Unreadable(_))));
        assert!(matches!(
            decode(&NATIVE[..NATIVE.len() / 2]),
            Err(SegmentError::Unreadable(_))
        ));
        let mut junk = NATIVE.to_vec();
        junk.push(0);
        assert!(matches!(decode(&junk), Err(SegmentError::Unreadable(_))));
    }

    fn deflate(text: &str) -> Vec<u8> {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(text.as_bytes()).unwrap();
        e.finish().unwrap()
    }

    fn raw(body: Value) -> Vec<u8> {
        let mut base = json!({
            "format": 1, "device": "dev-a", "month": "2026-10", "first_seq": 1, "last_seq": 1,
            "count": 1, "volumes": ["vol-1"],
        });
        if let (Value::Object(m), Value::Object(b)) = (&mut base, body) {
            m.extend(b);
        }
        deflate(&base.to_string())
    }

    const T: i64 = 1_791_028_800_000;

    /// The reader's `segment-codec.test.ts` "rejects decodable files with malformed
    /// cells" cases, plus the resolve ones.
    #[test]
    fn malformed_cells_as_in_the_readers_tests() {
        let cases = [
            json!({"rows": [[2, "1", T, 0]]}),
            json!({"rows": [[2, 1, null, 0]]}),
            json!({"count": 2, "last_seq": 1, "rows": [[2, 1, T, 0], [2, 0, 1, 0]]}),
            json!({"rows": [[0, 1, T, 0, 1, 1, "x", 0, 5, 0, 0, 1, 1]]}),
            json!({"rows": [[0, 1, T, 0, "one", 1, [1], 0, 5, 0, 0, 1, 1]]}),
            json!({"first_seq": "1", "rows": [[2, 1, T, 0]]}),
            json!({"rows": [[1, 1, T, 0, "x", 0]]}),
            json!({"rows": [[5, 1, T, 0, 0, 7, 0]]}),
            json!({"targets": ["dev-b"], "rows": [[5, 1, T, 0, 1, 7, 0]]}),
            json!({"targets": [""], "rows": [[5, 1, T, 0, 0, 7, 0]]}),
            json!({"targets": ["dev-b"], "rows": [[5, 1, T, 0, "0", 7, 0]]}),
            json!({"targets": [7], "rows": [[5, 1, T, 0, 0, 7, 0]]}),
            json!({"targets": ["dev-b"], "rows": [[5, 1, T, 0, 0, 0, 0]]}),
            json!({"targets": ["dev-b"], "rows": [[5, 1, T, 0, 0, 7, 3]]}),
            json!({"legacy": true, "targets": ["dev-b"], "rows": [[5, 1, T, 0, 0, 7, 0]]}),
            // And: an unknown kind, a bad volume index, a row that is not an array, a
            // page row too short for its viewport, a count that disagrees, a seq range
            // that disagrees with the header, an unknown position answer.
            json!({"rows": [[6, 1, T, 0]]}),
            json!({"rows": [[2, 1, T, 1]]}),
            json!({"rows": [{"kind": 2}]}),
            json!({"rows": [[0, 1, T, 0, 1, 1, [1], 0, 5, 0, 0]]}),
            json!({"count": 2, "rows": [[2, 1, T, 0]]}),
            json!({"last_seq": 2, "rows": [[2, 1, T, 0]]}),
            json!({"rows": [[4, 1, T, 0, 3, 0, 1]]}),
        ];
        for body in cases {
            let r = decode(&raw(body.clone()));
            assert!(
                matches!(r, Err(SegmentError::Malformed(_))),
                "{body}: {r:?}"
            );
        }
    }

    /// What the reader's `decodeSegment` answers for these (run in Node against
    /// develop 085453c2): all accepted.
    #[test]
    fn accepts_what_the_reader_accepts() {
        let ok = |text: &str| {
            let s = decode(&deflate(text)).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(s.events.len(), 1, "{text}");
            s
        };
        let base = r#""device":"d","month":"2026-10","first_seq":1,"last_seq":1,"count":1,"volumes":["v"]"#;
        ok(&format!(
            "\u{feff}{{\"format\":1,{base},\"rows\":[[2,1,5,0]]}}"
        ));
        // A string index ("0" names volume 0), floats where integers go.
        ok(&format!("{{\"format\":1,{base},\"rows\":[[2,1,5,\"0\"]]}}"));
        ok(
            r#"{"format":1.0,"device":"d","month":"2026-10","first_seq":1,"last_seq":1,"count":1.0,"volumes":["v"],"rows":[[2.0,1.0,5,0.0]]}"#,
        );
        // The rows before the header, a duplicate key (the last wins), trailing space.
        ok(&format!("{{\"rows\":[[2,1,5,0]],\"format\":1,{base}}}"));
        ok(&format!(
            "{{\"format\":2,\"format\":1,{base},\"rows\":[[2,1,5,0]]}}  \n"
        ));
        // Cells past the 13th, a layout out of range, an unreferenced non-string volume.
        let s = ok(
            r#"{"format":1,"device":"d","month":"2026-10","first_seq":1,"last_seq":1,"count":1,"volumes":["v",7],"rows":[[0,1,5,0,1,1,[1,2],0,-1,9,"1",-1,"x","extra"]]}"#,
        );
        match s.events[0].payload {
            Payload::Page {
                layout,
                orientation,
                viewport,
                dwell_ms,
                ..
            } => {
                assert_eq!(layout, Layout::Unknown);
                assert_eq!(orientation, Orientation::Landscape);
                assert_eq!((viewport, dwell_ms), (None, None));
            }
            _ => panic!("a page"),
        }
        // An array at the top is "unsupported segment format undefined" there.
        assert_eq!(
            decode(&deflate("[1]")),
            Err(SegmentError::UnsupportedFormat)
        );
        assert_eq!(
            decode(&deflate("null")),
            Err(SegmentError::Malformed("not a segment"))
        );
        // Invalid UTF-8 inside a string is read with replacement, as Response.text().
        let mut bytes = format!("{{\"format\":1,{base},\"rows\":[[2,1,5,0]],\"x\":\"").into_bytes();
        bytes.extend([0xff, b'"', b'}']);
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&bytes).unwrap();
        assert!(decode(&e.finish().unwrap()).is_ok());
    }

    #[test]
    fn legacy_segments_need_contiguous_devices() {
        let text = json!({
            "format": 1, "device": "dev-a", "month": "legacy", "first_seq": 0, "last_seq": 0,
            "count": 3, "legacy": true, "volumes": ["v1", "v2"],
            "rows": [[2, 5, 5, 0], [2, 3, 3, 1], [2, 9, 9, 0]],
        });
        assert_eq!(
            decode(&deflate(&text.to_string())),
            Err(SegmentError::Malformed("device rows are not contiguous"))
        );
        let ok = json!({
            "format": 1, "device": "dev-a", "month": "legacy", "first_seq": 0, "last_seq": 0,
            "count": 3, "legacy": true, "volumes": ["v1", "v2"],
            "rows": [[2, 5, 5, 0], [2, 4, 4, 0], [2, 3, -6, 1]],
        });
        let s = decode(&deflate(&ok.to_string())).unwrap();
        let seqs: Vec<(String, i64, i64)> = s
            .events
            .iter()
            .map(|e| (s.device_of(e), e.seq, e.t))
            .collect();
        assert_eq!(
            seqs,
            [
                ("legacy:v1".to_string(), 5, 5),
                ("legacy:v1".to_string(), 9, 9),
                ("legacy:v2".to_string(), 3, 3)
            ]
        );
    }

    #[test]
    fn bounded_against_inflation_and_row_floods() {
        // 64 MiB of spaces deflate to a few KiB: refused past the bound, quickly.
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        let chunk = vec![b' '; 1 << 20];
        e.write_all(b"{\"format\":1").unwrap();
        for _ in 0..64 {
            e.write_all(&chunk).unwrap();
        }
        e.write_all(b"}").unwrap();
        let bomb = e.finish().unwrap();
        assert!(bomb.len() < 200_000, "{}", bomb.len());
        assert_eq!(decode(&bomb), Err(SegmentError::TooLarge));
        // Rows past the bound.
        let rows: Vec<Value> = (0..20).map(|_| json!([2, 1, 0, 0])).collect();
        let text = json!({"format": 1, "device": "d", "month": "2026-10", "first_seq": 1,
            "last_seq": 20, "count": 20, "volumes": ["v"], "rows": rows});
        let opts = Options {
            max_events: 10,
            ..Options::default()
        };
        assert_eq!(
            decode_with(&deflate(&text.to_string()), opts),
            Err(SegmentError::TooLarge)
        );
        assert!(decode(&deflate(&text.to_string())).is_ok());
    }
}
