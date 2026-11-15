//! The raw initializers of an ONNX file, found without a protobuf dependency. Always built
//! (no libtorch), so it is tested in the normal workspace build; the libtorch side
//! (`Weights::load_onnx`) then reads each recorded byte range as the safetensors path does.
//!
//! The walk follows `ModelProto.graph` (field 7) → `GraphProto.initializer` (field 5) →
//! `TensorProto` fields 1 (dims, plain or packed), 2 (data_type), 8 (name), 9 (raw_data)
//! and 14 (data_location). Every other field is skipped by its wire type (0, 1, 2 and 5;
//! groups, 3 and 4, are refused), and every length is checked against the end of the
//! message holding it, so nothing is read past `size`. The raw bytes are never read here:
//! their offset and length are recorded and the reader seeks past them. A wanted
//! initializer must be FLOAT, held in `raw_data` (not external) with exactly
//! `4 · prod(dims)` bytes, and appear once; an unwanted one is skipped without these checks.
//!
//! Pass a `BufReader` around a file: the walk reads varints a byte at a time and skips with
//! `seek_relative`, which keeps the buffer.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Seek, SeekFrom};

/// `TensorProto.DataType.FLOAT`.
pub const FLOAT: i32 = 1;
/// `TensorProto.DataLocation.EXTERNAL`.
const EXTERNAL: i32 = 1;
/// More dims than this is no tensor we bind (only the first ones are kept while walking).
const MAX_RANK: usize = 32;

const VARINT: u64 = 0;
const FIXED64: u64 = 1;
const LEN: u64 = 2;
const FIXED32: u64 = 5;

/// One initializer whose data is the `len` bytes at `offset` in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnnxTensor {
    pub name: String,
    pub dims: Vec<i64>,
    pub data_type: i32,
    pub offset: u64,
    pub len: u64,
}

/// Finds every `wanted` initializer of the ONNX model in `r` (`size` bytes long), or says
/// why one cannot be bound.
pub fn scan<R: Read + Seek>(
    r: &mut R,
    size: u64,
    wanted: &HashSet<String>,
) -> Result<HashMap<String, OnnxTensor>, String> {
    r.seek(SeekFrom::Start(0))
        .map_err(|e| format!("reading the ONNX file: {e}"))?;
    let mut w = Walker {
        r,
        pos: 0,
        name_max: wanted.iter().map(String::len).max().unwrap_or(0),
    };
    let mut found = HashMap::new();
    while w.pos < size {
        let (field, wire) = w.tag(size)?;
        if (field, wire) == (7, LEN) {
            let end = w.len_end(size)?;
            w.graph(end, wanted, &mut found)?;
        } else {
            w.skip_field(wire, size)?;
        }
    }
    let mut missing: Vec<&String> = wanted.iter().filter(|n| !found.contains_key(*n)).collect();
    missing.sort();
    if let Some(first) = missing.first() {
        let more = match missing.len() - 1 {
            0 => String::new(),
            n => format!(" (and {n} more)"),
        };
        return Err(format!("the ONNX file lacks {first}{more}"));
    }
    Ok(found)
}

/// A wanted initializer as walked, before its checks.
struct Walked {
    name: String,
    dims: Vec<i64>,
    rank: usize,
    data_type: i32,
    location: i32,
    raw: Option<(u64, u64)>,
}

impl Walked {
    fn check(self) -> Result<OnnxTensor, String> {
        let name = self.name;
        if self.data_type != FLOAT {
            return Err(format!(
                "initializer {name} is not FLOAT (data_type {})",
                self.data_type
            ));
        }
        if self.location == EXTERNAL {
            return Err(format!(
                "initializer {name} is external (its data is not inside the ONNX file)"
            ));
        }
        if self.rank > MAX_RANK {
            return Err(format!("initializer {name} has {} dims", self.rank));
        }
        let count = self
            .dims
            .iter()
            .try_fold(1u64, |n, &d| {
                u64::try_from(d).ok().and_then(|d| n.checked_mul(d))
            })
            .ok_or_else(|| format!("initializer {name} has dims {:?}", self.dims))?;
        let (offset, len) = self.raw.unwrap_or((0, 0));
        if count.checked_mul(4) != Some(len) {
            return Err(format!("{name}: {len} bytes for {count} floats"));
        }
        Ok(OnnxTensor {
            name,
            dims: self.dims,
            data_type: self.data_type,
            offset,
            len,
        })
    }
}

struct Walker<'a, R> {
    r: &'a mut R,
    /// Bytes consumed so far, i.e. the reader's position.
    pos: u64,
    /// A name longer than every wanted one is not read (it cannot be wanted).
    name_max: usize,
}

impl<R: Read + Seek> Walker<'_, R> {
    fn truncated(&self, what: &str) -> String {
        format!(
            "the ONNX file is truncated: {what} at byte {} runs past its end",
            self.pos
        )
    }

    fn io(&self, e: std::io::Error) -> String {
        if e.kind() == ErrorKind::UnexpectedEof {
            self.truncated("a read")
        } else {
            format!("reading the ONNX file at byte {}: {e}", self.pos)
        }
    }

    fn byte(&mut self, end: u64) -> Result<u8, String> {
        if self.pos >= end {
            return Err(self.truncated("a varint"));
        }
        let mut b = [0u8];
        self.r.read_exact(&mut b).map_err(|e| self.io(e))?;
        self.pos += 1;
        Ok(b[0])
    }

    fn varint(&mut self, end: u64) -> Result<u64, String> {
        let start = self.pos;
        let mut v = 0u64;
        for i in 0..10 {
            let b = self.byte(end)?;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(format!(
            "the ONNX file has a varint over 10 bytes at byte {start}"
        ))
    }

    fn tag(&mut self, end: u64) -> Result<(u64, u64), String> {
        let at = self.pos;
        let tag = self.varint(end)?;
        if tag >> 3 == 0 {
            return Err(format!("the ONNX file has field number 0 at byte {at}"));
        }
        Ok((tag >> 3, tag & 7))
    }

    /// Reads a length and returns where that field ends, which must not pass `end`.
    fn len_end(&mut self, end: u64) -> Result<u64, String> {
        let n = self.varint(end)?;
        match self.pos.checked_add(n) {
            Some(e) if e <= end => Ok(e),
            _ => Err(self.truncated(&format!("a {n}-byte field"))),
        }
    }

    fn skip_to(&mut self, to: u64) -> Result<(), String> {
        let delta = i64::try_from(to - self.pos)
            .map_err(|_| format!("the ONNX file skips {} bytes", to - self.pos))?;
        self.r.seek_relative(delta).map_err(|e| self.io(e))?;
        self.pos = to;
        Ok(())
    }

    fn skip(&mut self, n: u64, end: u64) -> Result<(), String> {
        match self.pos.checked_add(n) {
            Some(to) if to <= end => self.skip_to(to),
            _ => Err(self.truncated(&format!("a {n}-byte field"))),
        }
    }

    fn skip_field(&mut self, wire: u64, end: u64) -> Result<(), String> {
        match wire {
            VARINT => self.varint(end).map(drop),
            FIXED64 => self.skip(8, end),
            LEN => {
                let e = self.len_end(end)?;
                self.skip_to(e)
            }
            FIXED32 => self.skip(4, end),
            w => Err(format!(
                "the ONNX file has wire type {w} at byte {}: groups and unknown wire types are not read",
                self.pos
            )),
        }
    }

    fn graph(
        &mut self,
        end: u64,
        wanted: &HashSet<String>,
        found: &mut HashMap<String, OnnxTensor>,
    ) -> Result<(), String> {
        while self.pos < end {
            let (field, wire) = self.tag(end)?;
            if (field, wire) != (5, LEN) {
                self.skip_field(wire, end)?;
                continue;
            }
            let e = self.len_end(end)?;
            let Some(t) = self.tensor(e)? else { continue };
            if !wanted.contains(&t.name) {
                continue;
            }
            if found.contains_key(&t.name) {
                return Err(format!("the ONNX file holds {} twice", t.name));
            }
            let t = t.check()?;
            found.insert(t.name.clone(), t);
        }
        Ok(())
    }

    /// Walks one `TensorProto` ending at `end`; `None` when it has no name short enough to
    /// be wanted. Fields may come in any order; for a repeated singular field the last wins.
    fn tensor(&mut self, end: u64) -> Result<Option<Walked>, String> {
        let mut t = Walked {
            name: String::new(),
            dims: Vec::new(),
            rank: 0,
            data_type: 0,
            location: 0,
            raw: None,
        };
        let mut named = false;
        while self.pos < end {
            let (field, wire) = self.tag(end)?;
            match (field, wire) {
                (1, VARINT) => {
                    let d = self.varint(end)?;
                    t.push_dim(d);
                }
                (1, LEN) => {
                    let e = self.len_end(end)?;
                    while self.pos < e {
                        let d = self.varint(e)?;
                        t.push_dim(d);
                    }
                }
                // int32 fields arrive as sign-extended varints; truncation restores them.
                (2, VARINT) => t.data_type = self.varint(end)? as i32,
                (14, VARINT) => t.location = self.varint(end)? as i32,
                (8, LEN) => {
                    let e = self.len_end(end)?;
                    let n = e - self.pos;
                    named = false;
                    if n <= self.name_max as u64 {
                        let mut b = vec![0u8; n as usize];
                        self.r.read_exact(&mut b).map_err(|e| self.io(e))?;
                        self.pos = e;
                        if let Ok(s) = String::from_utf8(b) {
                            t.name = s;
                            named = true;
                        }
                    } else {
                        self.skip_to(e)?;
                    }
                }
                (9, LEN) => {
                    let e = self.len_end(end)?;
                    t.raw = Some((self.pos, e - self.pos));
                    self.skip_to(e)?;
                }
                _ => self.skip_field(wire, end)?,
            }
        }
        Ok(named.then_some(t))
    }
}

impl Walked {
    /// int64 dims arrive as two's-complement varints.
    fn push_dim(&mut self, d: u64) {
        self.rank += 1;
        if self.dims.len() < MAX_RANK {
            self.dims.push(d as i64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Cursor;

    fn varint(mut v: u64) -> Vec<u8> {
        let mut o = vec![];
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                o.push(b);
                return o;
            }
            o.push(b | 0x80)
        }
    }
    fn len_field(no: u64, body: &[u8]) -> Vec<u8> {
        [
            varint(no << 3 | 2),
            varint(body.len() as u64),
            body.to_vec(),
        ]
        .concat()
    }
    fn var_field(no: u64, v: u64) -> Vec<u8> {
        [varint(no << 3), varint(v)].concat()
    }
    fn tensor(name: &str, dims: &[u64], dtype: u64, raw: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut t: Vec<u8> = dims.iter().flat_map(|d| var_field(1, *d)).collect();
        t.extend(var_field(2, dtype));
        t.extend(len_field(8, name.as_bytes()));
        t.extend(len_field(9, raw));
        t.extend_from_slice(extra);
        t
    }
    fn model(tensors: &[Vec<u8>]) -> Vec<u8> {
        // fixed64, fixed32: skipped
        let mut g = [
            varint(99 << 3 | 1),
            vec![0; 8],
            varint(98 << 3 | 5),
            vec![0; 4],
        ]
        .concat();
        for t in tensors {
            g.extend(len_field(5, t));
        }
        [var_field(1, 8), len_field(7, &g)].concat()
    }
    fn want(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_raw_initializers() {
        let bytes = model(&[
            tensor("a", &[2, 3], 1, &[7; 24], &[]),
            tensor("b", &[1], 1, &[1; 4], &[]),
        ]);
        let found = scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &want(&["a", "b"]),
        )
        .unwrap();
        let a = &found["a"];
        assert_eq!((a.dims.clone(), a.len), (vec![2, 3], 24));
        assert_eq!(&bytes[a.offset as usize..][..24], &[7u8; 24]);
    }

    #[test]
    fn refuses_what_it_cannot_bind() {
        let check = |t: Vec<u8>, w: &[&str], msg: &str| {
            let b = model(&[t]);
            let e = scan(&mut Cursor::new(&b), b.len() as u64, &want(w)).unwrap_err();
            assert!(e.contains(msg), "{e}");
        };
        check(
            tensor("a", &[2], 10, &[0; 4], &[]),
            &["a"],
            "a is not FLOAT",
        );
        check(
            tensor("a", &[2], 1, &[0; 8], &var_field(14, 1)),
            &["a"],
            "a is external",
        );
        check(
            tensor("a", &[2, 3], 1, &[0; 20], &[]),
            &["a"],
            "a: 20 bytes for 6 floats",
        );
        check(tensor("a", &[1], 1, &[0; 4], &[]), &["a", "c"], "lacks c");
        let two = model(&[
            tensor("a", &[1], 1, &[0; 4], &[]),
            tensor("a", &[1], 1, &[0; 4], &[]),
        ]);
        let e = scan(&mut Cursor::new(&two), two.len() as u64, &want(&["a"])).unwrap_err();
        assert!(e.contains("a twice"), "{e}");
        let cut = &model(&[tensor("a", &[1], 1, &[0; 4], &[])])[..12];
        let e = scan(&mut Cursor::new(cut), cut.len() as u64, &want(&["a"])).unwrap_err();
        assert!(e.contains("truncated"), "{e}");
    }

    #[test]
    fn packed_dims_read_too() {
        let mut t = len_field(1, &[varint(2), varint(3)].concat());
        t.extend(var_field(2, 1));
        t.extend(len_field(8, b"p"));
        t.extend(len_field(9, &[0; 24]));
        let b = model(&[t]);
        let found = scan(&mut Cursor::new(&b), b.len() as u64, &want(&["p"])).unwrap();
        assert_eq!(found["p"].dims, vec![2, 3]);
    }

    #[test]
    fn unwanted_initializers_are_not_checked() {
        // a FLOAT16 one, an external one, a long name, a wrong byte count: none is wanted
        let bytes = model(&[
            tensor("half", &[2], 10, &[0; 4], &[]),
            tensor("ext", &[2], 1, &[], &var_field(14, 1)),
            tensor(&"x".repeat(5000), &[1], 1, &[0; 3], &[]),
            tensor("a", &[1], 1, &[9; 4], &[]),
        ]);
        let found = scan(&mut Cursor::new(&bytes), bytes.len() as u64, &want(&["a"])).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(&bytes[found["a"].offset as usize..][..4], &[9u8; 4]);
        // nothing wanted: the whole file is still walked and bounded
        assert!(
            scan(&mut Cursor::new(&bytes), bytes.len() as u64, &want(&[]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fields_in_any_order_and_the_last_name_wins() {
        let mut t = len_field(9, &[5; 8]);
        t.extend(len_field(8, b"old"));
        t.extend(var_field(2, 1));
        t.extend(var_field(1, 2));
        t.extend(len_field(8, b"a"));
        let b = model(&[t]);
        let found = scan(&mut Cursor::new(&b), b.len() as u64, &want(&["a", "old"]));
        assert!(found.unwrap_err().contains("lacks old"));
        let found = scan(&mut Cursor::new(&b), b.len() as u64, &want(&["a"])).unwrap();
        assert_eq!((found["a"].dims.clone(), found["a"].len), (vec![2], 8));
    }

    #[test]
    fn refuses_a_broken_file() {
        let err = |b: &[u8]| scan(&mut Cursor::new(b), b.len() as u64, &want(&["a"])).unwrap_err();
        // no raw_data at all (the data in float_data, field 4)
        let mut t = var_field(1, 2);
        t.extend(var_field(2, 1));
        t.extend(len_field(8, b"a"));
        t.extend(len_field(4, &[0; 8]));
        assert!(err(&model(&[t])).contains("a: 0 bytes for 2 floats"));
        // a negative dim (int64 -1 is a 10-byte varint)
        assert!(err(&model(&[tensor("a", &[u64::MAX], 1, &[], &[])])).contains("dims [-1]"));
        // a group (wire type 3)
        let e = err(&[model(&[]), varint(3 << 3 | 3)].concat());
        assert!(e.contains("wire type 3"), "{e}");
        // an initializer whose length runs past the graph holding it
        let mut g = len_field(5, &tensor("a", &[1], 1, &[0; 4], &[]));
        g[1] += 4;
        let b = [len_field(7, &g), vec![0; 4]].concat();
        assert!(err(&b).contains("truncated"));
        // a raw_data length past the end of the file
        let b = model(&[tensor("a", &[1], 1, &[0; 4], &[])]);
        assert!(err(&b[..b.len() - 1]).contains("truncated"));
        // a varint that never ends
        assert!(err(&[0xff; 11]).contains("varint over 10 bytes"));
        // several missing names
        let b = model(&[]);
        let e = scan(&mut Cursor::new(&b), b.len() as u64, &want(&["c", "b"])).unwrap_err();
        assert_eq!(e, "the ONNX file lacks b (and 1 more)");
    }

    /// The mirrored AnimeText file through a `BufReader`, against a dump of every
    /// initializer made with the `onnx` package (name → dims, data_type, len, sha256 of
    /// raw_data). Run with
    /// `BUNKO_ANIMETEXT_ONNX=<animetext_yolo12x.onnx> BUNKO_ONNX_EXPECT=<dump.json>
    /// cargo test -p bunko-torch --lib onnx_weights -- --ignored`.
    #[test]
    #[ignore = "needs the AnimeText ONNX file and an onnx-package dump of it"]
    fn the_mirrored_animetext_file() {
        use sha2::{Digest, Sha256};
        use std::io::{BufReader, Read, Seek, SeekFrom};
        let path = std::env::var("BUNKO_ANIMETEXT_ONNX").expect("BUNKO_ANIMETEXT_ONNX");
        let expect: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(
            &std::fs::read(std::env::var("BUNKO_ONNX_EXPECT").expect("BUNKO_ONNX_EXPECT")).unwrap(),
        )
        .unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let mut r = BufReader::new(std::fs::File::open(&path).unwrap());
        let floats: HashSet<String> = expect
            .iter()
            .filter(|(_, v)| v["data_type"] == 1)
            .map(|(k, _)| k.clone())
            .collect();
        let found = scan(&mut r, size, &floats).unwrap();
        assert_eq!(found.len(), floats.len());
        let mut buf = Vec::new();
        for (name, t) in &found {
            let v = &expect[name];
            let dims: Vec<i64> = serde_json::from_value(v["dims"].clone()).unwrap();
            assert_eq!(t.dims, dims, "{name}");
            assert_eq!(t.len, v["len"].as_u64().unwrap(), "{name}");
            r.seek(SeekFrom::Start(t.offset)).unwrap();
            buf.resize(t.len as usize, 0);
            r.read_exact(&mut buf).unwrap();
            let sha: String = Sha256::digest(&buf)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(sha, v["sha256"].as_str().unwrap(), "{name}");
        }
        assert_eq!(found["onnx::Mul_2760"].dims, vec![1, 768, 1, 1]);
        let ints: Vec<&String> = expect.keys().filter(|k| !floats.contains(*k)).collect();
        if let Some(i) = ints.first() {
            let e = scan(&mut r, size, &want(&[i.as_str()])).unwrap_err();
            assert!(e.contains("is not FLOAT"), "{e}");
        }
        eprintln!(
            "{} FLOAT initializers, {} bytes, {} others",
            found.len(),
            found.values().map(|t| t.len).sum::<u64>(),
            ints.len()
        );
    }
}
