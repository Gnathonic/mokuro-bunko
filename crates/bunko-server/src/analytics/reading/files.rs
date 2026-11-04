//! The reader's per-user files, as the reading worker reads them back: which file a
//! per-user path is, and parsers for `positions.json`, `volume-data.json` and
//! `history/<device>/device.json` that keep only what the analytics use (never the
//! page-turn logs inside `volume-data.json`) and accept what the reader accepts.

use bunko_db::analytics::reading::PositionEntry;
use serde::Deserializer as _;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Positions newer than now plus this are a fast clock's and dropped, as the reader
/// drops them (`FUTURE_TOLERANCE_MS`).
pub const FUTURE_TOLERANCE_MS: i64 = 5 * 60 * 1000;
/// Most entries kept from one `positions.json` (the reader keeps 30 days of them).
pub const MAX_POSITIONS: usize = 20_000;
/// Most records kept from one `volume-data.json`.
pub const MAX_RECORDS: usize = 100_000;
/// Longest volume id kept (the reader's are 36-character uuids).
pub const MAX_ID_CHARS: usize = 128;
/// Longest title or label kept.
const MAX_TEXT_CHARS: usize = 256;

/// What a per-user path is (`history/...` as the reader's `parseHistoryPath` reads it:
/// exactly three segments, case-sensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind<'a> {
    Positions,
    VolumeData,
    /// `history/<device>/<YYYY-MM>.events`
    Month {
        device: &'a str,
        month: &'a str,
    },
    /// `history/<device>/legacy.events`
    Legacy {
        device: &'a str,
    },
    /// `history/<device>/device.json`
    DeviceFacts {
        device: &'a str,
    },
    /// `profiles.json`, `goals.json` (sync bytes only) and anything else.
    Other,
}

/// `isDeviceId`: `^[A-Za-z0-9-]{1,64}$`.
pub fn is_device_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_month_file(name: &str) -> Option<&str> {
    let month = name.strip_suffix(".events")?;
    let b = month.as_bytes();
    let ok = b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && matches!((b[5], b[6]), (b'0', b'1'..=b'9') | (b'1', b'0'..=b'2'));
    ok.then_some(month)
}

pub fn kind_of(rel: &str) -> FileKind<'_> {
    let rel = rel.trim_matches('/');
    match rel {
        "positions.json" => return FileKind::Positions,
        "volume-data.json" => return FileKind::VolumeData,
        _ => {}
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let [dir, device, name] = parts[..] else {
        return FileKind::Other;
    };
    if dir != "history" || !is_device_id(device) {
        return FileKind::Other;
    }
    match name {
        "device.json" => FileKind::DeviceFacts { device },
        "legacy.events" => FileKind::Legacy { device },
        _ => match is_month_file(name) {
            Some(month) => FileKind::Month { device, month },
            None => FileKind::Other,
        },
    }
}

/// Why a file was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// Not JSON, or not the shape the reader writes.
    Malformed(String),
    /// A `version` this server does not know.
    UnknownFormat,
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Malformed(e) => write!(f, "malformed: {e}"),
            FileError::UnknownFormat => write!(f, "unknown format"),
        }
    }
}

fn json_error(e: serde_json::Error) -> FileError {
    FileError::Malformed(e.to_string())
}

/// A JSON value, as far as these files need it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum Val {
    #[default]
    Absent,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// An array or an object (not kept).
    Other,
}

impl Val {
    /// JavaScript truthiness (`!!value`; arrays and objects are truthy).
    fn truthy(&self) -> bool {
        match self {
            Val::Absent | Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(v) => *v != 0.0 && !v.is_nan(),
            Val::Str(s) => !s.is_empty(),
            Val::Other => true,
        }
    }

    fn number(&self) -> Option<f64> {
        match self {
            Val::Num(v) if v.is_finite() => Some(*v),
            _ => None,
        }
    }

    fn text(self) -> Option<String> {
        match self {
            Val::Str(s) => Some(cut(&s)),
            _ => None,
        }
    }
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_TEXT_CHARS).collect()
}

struct ValSeed;

impl<'de> DeserializeSeed<'de> for ValSeed {
    type Value = Val;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Val, D::Error> {
        d.deserialize_any(ValVisitor)
    }
}

struct ValVisitor;

impl<'de> Visitor<'de> for ValVisitor {
    type Value = Val;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Val, E> {
        Ok(Val::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Val, E> {
        Ok(Val::Num(v as f64))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Val, E> {
        Ok(Val::Num(v as f64))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Val, E> {
        Ok(Val::Num(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Val, E> {
        Ok(Val::Str(v.to_string()))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Val, E> {
        Ok(Val::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Val, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Val::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Val, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Val::Other)
    }
}

/// Parse `bytes` (UTF-8 with replacement, a leading BOM dropped) with `visitor` over
/// its top-level value.
fn parse<T, V>(bytes: &[u8], visitor: V) -> Result<T, FileError>
where
    V: for<'de> Visitor<'de, Value = T>,
{
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut de = serde_json::Deserializer::from_str(text);
    let v = (&mut de).deserialize_any(visitor).map_err(json_error)?;
    de.end().map_err(json_error)?;
    Ok(v)
}

/// `isCount`: a non-negative integer.
fn count(v: &Val) -> Option<i64> {
    match v {
        Val::Num(x) if x.fract() == 0.0 && *x >= 0.0 && *x <= i64::MAX as f64 => Some(*x as i64),
        _ => None,
    }
}

/// `positions.json` (`parsePositionsFile`): `{"version": 1, "positions": {"<volume>":
/// [t, page, chars, "<device>"]}}`; malformed entries are left out, as the reader
/// leaves them out, and so are entries stamped past `now_ms` plus
/// [`FUTURE_TOLERANCE_MS`].
pub fn parse_positions(bytes: &[u8], now_ms: i64) -> Result<Vec<PositionEntry>, FileError> {
    let (version, entries) = parse(bytes, PositionsVisitor { now_ms })?;
    if version != Val::Num(1.0) {
        return Err(FileError::UnknownFormat);
    }
    entries.ok_or_else(|| FileError::Malformed("positions is not an object".into()))
}

struct PositionsVisitor {
    now_ms: i64,
}

type PositionsParsed = (Val, Option<Vec<PositionEntry>>);

impl<'de> Visitor<'de> for PositionsVisitor {
    type Value = PositionsParsed;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a positions file")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PositionsParsed, A::Error> {
        let mut version = Val::Absent;
        let mut entries = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "version" => version = map.next_value_seed(ValSeed)?,
                "positions" => {
                    entries = map.next_value_seed(EntriesSeed {
                        now_ms: self.now_ms,
                    })?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok((version, entries))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<PositionsParsed, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok((Val::Absent, None))
    }
    fn visit_unit<E: de::Error>(self) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<PositionsParsed, E> {
        Ok((Val::Absent, None))
    }
}

struct EntriesSeed {
    now_ms: i64,
}

impl<'de> DeserializeSeed<'de> for EntriesSeed {
    type Value = Option<Vec<PositionEntry>>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for EntriesSeed {
    type Value = Option<Vec<PositionEntry>>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the positions")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(volume) = map.next_key::<String>()? {
            let cells = map.next_value_seed(TupleSeed)?;
            if out.len() >= MAX_POSITIONS
                || volume.is_empty()
                || volume.chars().count() > MAX_ID_CHARS
            {
                continue;
            }
            let Some([t, page, chars, device]) = cells else {
                continue;
            };
            let (Some(t), Some(page), Some(chars), Val::Str(device)) =
                (count(&t), count(&page), count(&chars), device)
            else {
                continue;
            };
            if device.is_empty() || t > self.now_ms.saturating_add(FUTURE_TOLERANCE_MS) {
                continue;
            }
            out.push(PositionEntry {
                volume_uuid: volume,
                t_ms: t,
                page,
                chars,
                device: cut(&device),
            });
        }
        Ok(Some(out))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// An array of exactly four values, or `None`.
struct TupleSeed;

impl<'de> DeserializeSeed<'de> for TupleSeed {
    type Value = Option<[Val; 4]>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for TupleSeed {
    type Value = Option<[Val; 4]>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a position")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut cells: [Val; 4] = Default::default();
        let mut n = 0usize;
        while let Some(v) = seq.next_element_seed(ValSeed)? {
            if let Some(slot) = cells.get_mut(n) {
                *slot = v;
            }
            n += 1;
        }
        Ok((n == 4).then_some(cells))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// One `volume-data.json` record, as the reader's `VolumeData` constructor reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeRecord {
    pub volume_uuid: String,
    /// `progress` (the page reached; not a number reads as 0).
    pub progress: f64,
    pub chars: f64,
    /// `!!completed`.
    pub completed: bool,
    pub completed_at: Option<String>,
    pub time_read_min: f64,
    pub last_progress_update: Option<String>,
    pub series_uuid: Option<String>,
    pub series_title: Option<String>,
    pub volume_title: Option<String>,
    /// A `deletedOn` tombstone: the reader dropped this volume.
    pub deleted: bool,
}

/// What `volume-data.json` holds for the analytics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeData {
    pub records: Vec<VolumeRecord>,
    /// The user's own device names (`tracking.devices[<id>].name`).
    pub device_names: Vec<(String, String)>,
    /// Records past [`MAX_RECORDS`], left out.
    pub dropped: u64,
}

/// `volume-data.json`: an object keyed by volume id (the reserved `series` and
/// `tracking` sections are not volumes); every field but the few the funnel reads is
/// skipped unparsed (the page-turn logs above all).
pub fn parse_volume_data(bytes: &[u8]) -> Result<VolumeData, FileError> {
    parse(bytes, VolumeDataVisitor)?
        .ok_or_else(|| FileError::Malformed("volume-data.json is not an object".into()))
}

struct VolumeDataVisitor;

impl<'de> Visitor<'de> for VolumeDataVisitor {
    type Value = Option<VolumeData>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("volume data")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = VolumeData::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "tracking" => out.device_names = map.next_value_seed(TrackingSeed)?,
                "series" | "" => {
                    map.next_value::<IgnoredAny>()?;
                }
                _ => {
                    let record = map.next_value_seed(RecordSeed)?;
                    let Some(mut r) = record else {
                        continue;
                    };
                    if key.chars().count() > MAX_ID_CHARS {
                        continue;
                    }
                    if out.records.len() >= MAX_RECORDS {
                        out.dropped += 1;
                        continue;
                    }
                    r.volume_uuid = key;
                    out.records.push(r);
                }
            }
        }
        Ok(Some(out))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// One volume's record; `None` for a value that is not an object.
struct RecordSeed;

impl<'de> DeserializeSeed<'de> for RecordSeed {
    type Value = Option<VolumeRecord>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RecordSeed {
    type Value = Option<VolumeRecord>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a volume record")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut r = VolumeRecord::default();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let field = match key.as_ref() {
                "progress" | "chars" | "completed" | "completedAt" | "timeReadInMinutes"
                | "lastProgressUpdate" | "series_uuid" | "series_title" | "volume_title"
                | "deletedOn" => map.next_value_seed(ValSeed)?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            match key.as_ref() {
                "progress" => r.progress = field.number().unwrap_or(0.0),
                "chars" => r.chars = field.number().unwrap_or(0.0),
                "completed" => r.completed = field.truthy(),
                "completedAt" => r.completed_at = field.text(),
                "timeReadInMinutes" => r.time_read_min = field.number().unwrap_or(0.0),
                "lastProgressUpdate" => r.last_progress_update = field.text(),
                "series_uuid" => r.series_uuid = field.text(),
                "series_title" => r.series_title = field.text(),
                "volume_title" => r.volume_title = field.text(),
                _ => r.deleted = field.truthy(),
            }
        }
        Ok(Some(r))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// `tracking.devices[<id>].name` of each device the user named.
struct TrackingSeed;

impl<'de> DeserializeSeed<'de> for TrackingSeed {
    type Value = Vec<(String, String)>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for TrackingSeed {
    type Value = Vec<(String, String)>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the tracking section")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "devices" {
                out = map.next_value_seed(DeviceNamesSeed)?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(out)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
}

struct DeviceNamesSeed;

impl<'de> DeserializeSeed<'de> for DeviceNamesSeed {
    type Value = Vec<(String, String)>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DeviceNamesSeed {
    type Value = Vec<(String, String)>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("device labels")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(id) = map.next_key::<String>()? {
            let name = map.next_value_seed(NameSeed)?;
            if let Some(name) = name
                && is_device_id(&id)
                && out.len() < 256
            {
                out.push((id, name));
            }
        }
        Ok(out)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
}

/// A `DeviceLabel`'s trimmed, non-empty `name`.
struct NameSeed;

impl<'de> DeserializeSeed<'de> for NameSeed {
    type Value = Option<String>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NameSeed {
    type Value = Option<String>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a device label")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut name = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "name" {
                name = map
                    .next_value_seed(ValSeed)?
                    .text()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(name)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// `history/<device>/device.json` (`DeviceFacts`), as the reader's importer checks it:
/// the folder's device, a known class, `first_seen`/`last_seen` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFacts {
    pub class: String,
    pub os: Option<String>,
    pub browser: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
}

const DEVICE_CLASSES: [(&str, &str); 5] = [
    ("phone", "Phone"),
    ("tablet", "Tablet"),
    ("laptop", "Laptop"),
    ("desktop", "Desktop"),
    ("unknown", "Device"),
];

impl DeviceFacts {
    /// The reader's generated label: `<class> · <OS> · <browser>`, what is unknown
    /// skipped (`generatedLabel`).
    pub fn label(&self) -> String {
        let os = self.os.as_deref().filter(|s| !s.is_empty());
        let browser = self.browser.as_deref().filter(|s| !s.is_empty());
        if self.class == "unknown" && os.is_none() && browser.is_none() {
            return "Unknown device".to_string();
        }
        let class = DEVICE_CLASSES
            .iter()
            .find(|(k, _)| *k == self.class)
            .map_or("Device", |(_, name)| *name);
        [Some(class), os, browser]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

pub fn parse_device_facts(bytes: &[u8], device: &str) -> Result<DeviceFacts, FileError> {
    let fields = parse(bytes, FactsVisitor)?
        .ok_or_else(|| FileError::Malformed("device.json is not an object".into()))?;
    let [id, class, os, browser, first_seen, last_seen] = fields;
    let ok_class = |c: &str| DEVICE_CLASSES.iter().any(|(k, _)| *k == c);
    match (id, class, first_seen, last_seen) {
        (Val::Str(id), Val::Str(class), Val::Str(first_seen), Val::Str(last_seen))
            if id == device && ok_class(&class) =>
        {
            Ok(DeviceFacts {
                class,
                os: os.text(),
                browser: browser.text(),
                first_seen,
                last_seen,
            })
        }
        _ => Err(FileError::Malformed(
            "device file does not match its folder".into(),
        )),
    }
}

struct FactsVisitor;

impl<'de> Visitor<'de> for FactsVisitor {
    type Value = Option<[Val; 6]>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("device facts")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out: [Val; 6] = Default::default();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let slot = match key.as_ref() {
                "device" => 0,
                "class" => 1,
                "os" => 2,
                "browser" => 3,
                "first_seen" => 4,
                "last_seen" => 5,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            out[slot] = map.next_value_seed(ValSeed)?;
        }
        Ok(Some(out))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
}

/// An ISO stamp (`2026-11-04T10:00:00.000Z`) as epoch seconds.
pub fn iso_seconds(text: &str) -> Option<i64> {
    bunko_library::isodate::fromisoformat_utc_micros(text).map(|us| us.div_euclid(1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn per_user_paths() {
        assert_eq!(kind_of("positions.json"), FileKind::Positions);
        assert_eq!(kind_of("/volume-data.json"), FileKind::VolumeData);
        assert_eq!(
            kind_of("history/dev-1/2026-10.events"),
            FileKind::Month {
                device: "dev-1",
                month: "2026-10"
            }
        );
        assert_eq!(
            kind_of("history/dev-1/legacy.events"),
            FileKind::Legacy { device: "dev-1" }
        );
        assert_eq!(
            kind_of("history/dev-1/device.json"),
            FileKind::DeviceFacts { device: "dev-1" }
        );
        for other in [
            "profiles.json",
            "goals.json",
            "history/dev-1/2026-13.events",
            "history/dev-1/2026-00.events",
            "history/dev_1/2026-10.events",
            "history/dev-1/x/2026-10.events",
            "History/dev-1/2026-10.events",
            "history/dev-1/2026-1.events",
        ] {
            assert_eq!(kind_of(other), FileKind::Other, "{other}");
        }
        assert!(is_device_id(&"a".repeat(64)));
        assert!(!is_device_id(&"a".repeat(65)));
        assert!(!is_device_id("legacy:v"));
    }

    #[test]
    fn positions_as_the_reader_parses_them() {
        let text = format!(
            r#"{{"version":1,"positions":{{
              "u1":[{t},12,3400,"dev-a"],
              "u2":[{t},1.5,0,"dev-a"],
              "u3":[{t},2,0,""],
              "u4":[{t},2,0,"dev-b","extra"],
              "u5":[{future},2,0,"dev-b"],
              "u6":{{"t":1}},
              "":[{t},2,0,"dev-b"],
              "u7":[{t},0,0,"dev-c"]}}}}"#,
            t = NOW - 1000,
            future = NOW + FUTURE_TOLERANCE_MS + 1
        );
        let e = parse_positions(text.as_bytes(), NOW).unwrap();
        let ids: Vec<&str> = e.iter().map(|e| e.volume_uuid.as_str()).collect();
        assert_eq!(ids, ["u1", "u7"]);
        assert_eq!(
            (e[0].page, e[0].chars, e[0].device.as_str()),
            (12, 3400, "dev-a")
        );
        assert_eq!(
            parse_positions(br#"{"version":2,"positions":{}}"#, NOW),
            Err(FileError::UnknownFormat)
        );
        assert!(matches!(
            parse_positions(b"not json", NOW),
            Err(FileError::Malformed(_))
        ));
        assert!(matches!(
            parse_positions(br#"{"version":1,"positions":[]}"#, NOW),
            Err(FileError::Malformed(_))
        ));
    }

    #[test]
    fn volume_data_keeps_only_the_funnel_fields() {
        let text = r#"{
          "series": {"s1": {"x": 1}},
          "tracking": {"devices": {"dev-a": {"name": "  My tablet ", "lastUpdated": "x"},
                                   "dev-b": {"merged_into": "dev-a"}, "bad id": {"name": "n"}}},
          "u1": {"progress": 40, "chars": 9000, "completed": true,
                 "completedAt": "2026-11-01T10:00:00.000Z", "timeReadInMinutes": 61.5,
                 "lastProgressUpdate": "2026-11-02T10:00:00.000Z",
                 "series_uuid": "s1", "series_title": "Kingdom", "volume_title": "Kingdom 01",
                 "recentPageTurns": [[1, 2, 3], [4, 5, 6]], "sessions": [{"a": 1}],
                 "settings": {"rightToLeft": true}},
          "u2": {"progress": "12", "completed": 0, "deletedOn": "2026-11-01T00:00:00Z"},
          "u3": 7
        }"#;
        let v = parse_volume_data(text.as_bytes()).unwrap();
        assert_eq!(
            v.device_names,
            [("dev-a".to_string(), "My tablet".to_string())]
        );
        assert_eq!(v.records.len(), 2);
        let r = &v.records[0];
        assert_eq!(r.volume_uuid, "u1");
        assert_eq!((r.progress, r.chars, r.completed), (40.0, 9000.0, true));
        assert_eq!(r.time_read_min, 61.5);
        assert_eq!(r.series_title.as_deref(), Some("Kingdom"));
        assert_eq!(
            iso_seconds(r.completed_at.as_deref().unwrap()),
            Some(1_793_527_200)
        );
        let r = &v.records[1];
        assert_eq!((r.progress, r.completed, r.deleted), (0.0, false, true));
        assert!(parse_volume_data(b"[]").is_err());
    }

    #[test]
    fn device_facts_as_the_importer_checks_them() {
        let ok = br#"{"device":"dev-a","class":"tablet","os":"iOS","browser":"Safari",
            "first_seen":"2026-10-01T00:00:00.000Z","last_seen":"2026-11-01T00:00:00.000Z"}"#;
        let f = parse_device_facts(ok, "dev-a").unwrap();
        assert_eq!(f.label(), "Tablet · iOS · Safari");
        assert!(parse_device_facts(ok, "dev-b").is_err());
        let bare = br#"{"device":"d","class":"unknown","first_seen":"x","last_seen":"y"}"#;
        assert_eq!(
            parse_device_facts(bare, "d").unwrap().label(),
            "Unknown device"
        );
        let desktop = br#"{"device":"d","class":"desktop","browser":"Firefox","first_seen":"x","last_seen":"y"}"#;
        assert_eq!(
            parse_device_facts(desktop, "d").unwrap().label(),
            "Desktop · Firefox"
        );
        let bad_class = br#"{"device":"d","class":"toaster","first_seen":"x","last_seen":"y"}"#;
        assert!(parse_device_facts(bad_class, "d").is_err());
    }
}
