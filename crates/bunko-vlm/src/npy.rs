//! A minimal `.npy` reader: version 1/2/3 headers, C order, little-endian
//! `|u1`, `<f4`, `<f2`, `<i8`. That is all the exported host tables (and test fixtures) use.

use std::path::Path;

use crate::VlmError;

/// Element type of an `.npy` array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NpyDtype {
    U8,
    F32,
    F16,
    I64,
}

impl NpyDtype {
    fn size(self) -> usize {
        match self {
            NpyDtype::U8 => 1,
            NpyDtype::F32 => 4,
            NpyDtype::F16 => 2,
            NpyDtype::I64 => 8,
        }
    }
}

/// A loaded array: raw little-endian bytes plus shape.
#[derive(Debug)]
pub struct Npy {
    pub dtype: NpyDtype,
    pub shape: Vec<usize>,
    pub bytes: Vec<u8>,
}

impl Npy {
    pub fn load(path: &Path) -> Result<Npy, VlmError> {
        let data = std::fs::read(path).map_err(|e| VlmError::Io(path.display().to_string(), e))?;
        Npy::parse(&data).map_err(|why| VlmError::Asset(format!("{}: {why}", path.display())))
    }

    pub fn parse(data: &[u8]) -> Result<Npy, String> {
        if data.len() < 10 || &data[..6] != b"\x93NUMPY" {
            return Err("not an .npy file".into());
        }
        let (hlen, start) = match data[6] {
            1 => (usize::from(u16::from_le_bytes([data[8], data[9]])), 10),
            2 | 3 if data.len() >= 12 => (
                u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize,
                12,
            ),
            v => return Err(format!("unsupported .npy version {v}")),
        };
        let header = data.get(start..start + hlen).ok_or("truncated header")?;
        let header = std::str::from_utf8(header).map_err(|_| "header is not text")?;
        let dtype = match field(header, "descr")
            .ok_or("no descr")?
            .trim_matches(|c| c == '\'' || c == '"')
        {
            "|u1" => NpyDtype::U8,
            "<f4" => NpyDtype::F32,
            "<f2" => NpyDtype::F16,
            "<i8" => NpyDtype::I64,
            d => return Err(format!("unsupported dtype {d}")),
        };
        if field(header, "fortran_order")
            .ok_or("no fortran_order")?
            .trim()
            != "False"
        {
            return Err("fortran order is not supported".into());
        }
        let shape_txt = field(header, "shape").ok_or("no shape")?;
        let shape: Vec<usize> = shape_txt
            .trim_matches(|c| c == '(' || c == ')')
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<usize>()
                    .map_err(|_| format!("bad shape {shape_txt}"))
            })
            .collect::<Result<_, _>>()?;
        let n: usize = shape.iter().product();
        let body = &data[start + hlen..];
        if body.len() != n * dtype.size() {
            return Err(format!(
                "expected {} data bytes, found {}",
                n * dtype.size(),
                body.len()
            ));
        }
        Ok(Npy {
            dtype,
            shape,
            bytes: body.to_vec(),
        })
    }

    fn expect(&self, dtype: NpyDtype) -> Result<(), VlmError> {
        if self.dtype == dtype {
            Ok(())
        } else {
            Err(VlmError::Asset(format!(
                "expected {dtype:?} array, found {:?}",
                self.dtype
            )))
        }
    }

    pub fn into_f32(self) -> Result<Vec<f32>, VlmError> {
        self.expect(NpyDtype::F32)?;
        Ok(self
            .bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
    }

    /// An f32 or f16 array as f32 (f16 widened, which is exact).
    pub fn into_f32_widened(self) -> Result<Vec<f32>, VlmError> {
        match self.dtype {
            NpyDtype::F16 => Ok(self.into_f16()?.into_iter().map(f32::from).collect()),
            _ => self.into_f32(),
        }
    }

    pub fn into_f16(self) -> Result<Vec<half::f16>, VlmError> {
        self.expect(NpyDtype::F16)?;
        Ok(self
            .bytes
            .chunks_exact(2)
            .map(|b| half::f16::from_le_bytes([b[0], b[1]]))
            .collect())
    }

    pub fn into_u8(self) -> Result<Vec<u8>, VlmError> {
        self.expect(NpyDtype::U8)?;
        Ok(self.bytes)
    }

    pub fn into_i64(self) -> Result<Vec<i64>, VlmError> {
        self.expect(NpyDtype::I64)?;
        Ok(self
            .bytes
            .chunks_exact(8)
            .map(|b| i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
            .collect())
    }
}

/// The value text of `'key': value` in a Python dict literal (values here have no
/// commas except inside a parenthesised shape).
fn field<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let at = header.find(&format!("'{key}'"))?;
    let rest = header[at + key.len() + 2..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let end = if rest.starts_with('(') {
        rest.find(')')? + 1
    } else {
        rest.find([',', '}'])?
    };
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(descr: &str, shape: &str, body: &[u8]) -> Vec<u8> {
        let mut h = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape}, }}");
        while (10 + h.len() + 1) % 64 != 0 {
            h.push(' ');
        }
        h.push('\n');
        let mut v = b"\x93NUMPY\x01\x00".to_vec();
        v.extend_from_slice(&(h.len() as u16).to_le_bytes());
        v.extend_from_slice(h.as_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parses_shapes_and_types() {
        let body: Vec<u8> = [1.5f32, -2.0, 3.0, 4.0, 5.0, 6.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let a = Npy::parse(&make("<f4", "(2, 3)", &body)).unwrap();
        assert_eq!(a.shape, vec![2, 3]);
        assert_eq!(a.into_f32().unwrap()[1], -2.0);
        let b = Npy::parse(&make(
            "<i8",
            "(2,)",
            &[7, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0],
        ))
        .unwrap();
        assert_eq!(b.shape, vec![2]);
        assert_eq!(b.into_i64().unwrap(), vec![7, 9]);
        assert!(Npy::parse(&make("<f8", "(1,)", &[0; 8])).is_err());
        assert!(Npy::parse(&make("<f4", "(3,)", &[0; 8])).is_err());
    }
}
