//! OneDrive's QuickXorHash: the one content hash Graph guarantees on both OneDrive
//! Personal and OneDrive for work or school (`file.hashes.quickXorHash`, base64).
//!
//! The algorithm, as Microsoft's reference implementation (`QuickXorHash.cs`, linked
//! from the Graph `hashes` resource page) defines it:
//! - the state is a 160-bit circular bit vector, initially zero;
//! - the n-th input byte (0-based, over the whole stream) is XORed into the vector at
//!   bit position `(11 * n) mod 160`, wrapping from bit 159 to bit 0;
//! - the digest is the vector as 20 little-endian bytes, with the total input length
//!   (a little-endian `i64`) XORed into its last 8 bytes.
//!
//! Bytes 160 apart land on the same bit position, so [`QuickXor::update`] first XORs
//! the input into 160 column accumulators and only then shifts each into the state.

use std::io::Read;
use std::path::Path;

use base64::Engine;

const WIDTH_BITS: usize = 160;
const WIDTH_BYTES: usize = WIDTH_BITS / 8;
const SHIFT: usize = 11;

/// An incremental QuickXorHash.
#[derive(Debug, Clone)]
pub struct QuickXor {
    state: [u8; WIDTH_BYTES],
    /// The bit position the next input byte lands on.
    shift: usize,
    len: u64,
}

impl Default for QuickXor {
    fn default() -> Self {
        QuickXor::new()
    }
}

impl QuickXor {
    pub fn new() -> Self {
        QuickXor {
            state: [0; WIDTH_BYTES],
            shift: 0,
            len: 0,
        }
    }

    fn xor_at(&mut self, bit: usize, b: u8) {
        let i = bit / 8;
        let v = u16::from(b) << (bit % 8);
        self.state[i] ^= v as u8;
        self.state[(i + 1) % WIDTH_BYTES] ^= (v >> 8) as u8;
    }

    pub fn update(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut cols = [0u8; WIDTH_BITS];
        for chunk in data.chunks(WIDTH_BITS) {
            for (c, b) in cols.iter_mut().zip(chunk) {
                *c ^= b;
            }
        }
        let used = data.len().min(WIDTH_BITS);
        for (i, &c) in cols[..used].iter().enumerate() {
            if c != 0 {
                self.xor_at((self.shift + SHIFT * i) % WIDTH_BITS, c);
            }
        }
        self.shift = (self.shift + SHIFT * (data.len() % WIDTH_BITS)) % WIDTH_BITS;
        self.len += data.len() as u64;
    }

    /// The 20-byte digest.
    pub fn digest(&self) -> [u8; WIDTH_BYTES] {
        let mut out = self.state;
        for (o, l) in out[WIDTH_BYTES - 8..]
            .iter_mut()
            .zip(self.len.to_le_bytes())
        {
            *o ^= l;
        }
        out
    }

    /// The digest as Graph reports it: standard base64 with padding.
    pub fn finish_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.digest())
    }
}

/// The QuickXorHash (base64) of `data`.
pub fn quickxor_base64(data: &[u8]) -> String {
    let mut h = QuickXor::new();
    h.update(data);
    h.finish_base64()
}

/// The QuickXorHash (base64) of a local file, read in 1 MiB blocks. Blocking.
pub fn file_quickxor(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = QuickXor::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish_base64());
        }
        h.update(&buf[..n]);
    }
}

/// Two Graph hash strings name the same digest (base64 compared byte-exact after
/// decoding, so padding or whitespace differences do not matter).
pub fn same_hash(a: &str, b: &str) -> bool {
    let d = |s: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(s.trim())
            .ok()
    };
    match (d(a), d(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (length, input, digest), both base64. Generated from Microsoft's reference
    /// implementation; the same table backs rclone's `quickxorhash_test.go`.
    pub(crate) const VECTORS: &[(usize, &str, &str)] = &[
        (0, "", "AAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        (1, "Sg==", "SgAAAAAAAAAAAAAAAQAAAAAAAAA="),
        (2, "tbQ=", "taAFAAAAAAAAAAAAAgAAAAAAAAA="),
        (3, "0pZP", "0rDEEwAAAAAAAAAAAwAAAAAAAAA="),
        (8, "/aNzzCFPS/A=", "/RjFHJgRgicsAR4ACAAAAAAAAAA="),
        (
            16,
            "2fuxhBJXtpWFe8dOfdGeHw==",
            "O9tHLAghgSvYohKFyMMxnNCHaHg=",
        ),
        (
            20,
            "T6LYJIfDh81JrAK309H2JMJTXis=",
            "zBTHrspn3mEcohlJdIUAbjGNaNg=",
        ),
        (
            21,
            "DWAAX5/CIfrmErgZa8ot6ZraeSbu",
            "LR2Z0PjuRYGKQB/mhQAuMrAGZbQ=",
        ),
        (
            64,
            "Mb7EGva2rEE5fENDL85P+BsapHEEjv2/siVhKjvAQe02feExVOQSkfmuYzU/kTF1MaKjPmKF/w+cbvwfdWL8aQ==",
            "n1anP5NfvD4XDYWIeRPW3ZkPv1Y=",
        ),
        (
            111,
            "jyibxJSzO6ZiZ0O1qe3tG/bvIAYssvukh9suIT5wEy1JBINVgPiqdsTW0cOpP0aUfP7mgqLfADkzI/m/GgCuVhr8oFLrOCoTx1/psBOWwhltCbhUx51Icm9aH8tY4Z3ccU+6BKpYQkLCy0B/A9Zc",
            "hZfLIilSITC6N3e3tQ/iSgEzkto=",
        ),
        (
            128,
            "ikwCorI7PKWz17EI50jZCGbV9JU2E8bXVfxNMg5zdmqSZ2NlsQPp0kqYIPjzwTg1MBtfWPg53k0h0P2naJNEVgrqpoHTfV2b3pJ4m0zYPTJmUX4Bg/lOxcnCxAYKU29Y5F0U8Quz7ZXFBEweftXxJ7RS4r6N7BzJrPsLhY7hgck=",
            "imAoFvCWlDn4yVw3/oq1PDbbm6U=",
        ),
        (
            222,
            "PfxMcUd0vIW6VbHG/uj/Y0W6qEoKmyBD0nYebEKazKaKG+UaDqBEcmQjbfQeVnVLuodMoPp7P7TR1htX5n2VnkHh22xDyoJ8C/ZQKiSNqQfXvh83judf4RVr9exJCud8Uvgip6aVZTaPrJHVjQhMCp/dEnGvqg0oN5OVkM2qqAXvA0teKUDhgNM71sDBVBCGXxNOR2bpbD1iM4dnuT0ey4L+loXEHTL0fqMeUcEi2asgImnlNakwenDzz0x57aBwyq3AspCFGB1ncX4yYCr/OaCcS5OKi/00WH+wNQU3",
            "QX/YEpG0gDsmhEpCdWhsxDzsfVE=",
        ),
        (
            256,
            "qwGf2ESubE5jOUHHyc94ORczFYYbc2OmEzo+hBIyzJiNwAzC8PvJqtTzwkWkSslgHFGWQZR2BV5+uYTrYT7HVwRM40vqfj0dBgeDENyTenIOL1LHkjtDKoXEnQ0mXAHoJ8PjbNC93zi5TovVRXTNzfGEs5dpWVqxUzb5lc7dwkyvOluBw482mQ4xrzYyIY1t+//OrNi1ObGXuUw2jBQOFfJVj2Y6BOyYmfB1y36eBxi3zxeG5d5NYjm2GSh6e08QMAwu3zrINcqIzLOuNIiGXBtl7DjKt7b5wqi4oFiRpZsCyx2smhSrdrtK/CkdU6nDN+34vSR/M8rZpWQdBE7a8g==",
            "WYT9JY3JIo/pEBp+tIM6Gt2nyTM=",
        ),
        (
            333,
            "w0LGhqU1WXFbdavqDE4kAjEzWLGGzmTNikzqnsiXHx2KRReKVTxkv27u3UcEz9+lbMvYl4xFf2Z4aE1xRBBNd1Ke5C0zToSaYw5o4B/7X99nKK2/XaUX1byLow2aju2XJl2OpKpJg+tSJ2fmjIJTkfuYUz574dFX6/VXxSxwGH/xQEAKS5TCsBK3CwnuG1p5SAsQq3gGVozDWyjEBcWDMdy8/AIFrj/y03Lfc/RNRCQTAfZbnf2QwV7sluw4fH3XJr07UoD0YqN+7XZzidtrwqMY26fpLZnyZjnBEt1FAZWO7RnKG5asg8xRk9YaDdedXdQSJAOy6bWEWlABj+tVAigBxavaluUH8LOj+yfCFldJjNLdi90fVHkUD/m4Mr5OtmupNMXPwuG3EQlqWUVpQoYpUYKLsk7a5Mvg6UFkiH596y5IbJEVCI1Kb3D1",
            "e3+wo77iKcILiZegnzyUNcjCdoQ=",
        ),
    ];

    fn input(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap()
    }

    #[test]
    fn reference_vectors() {
        for &(n, inp, out) in VECTORS {
            let data = input(inp);
            assert_eq!(data.len(), n);
            assert_eq!(quickxor_base64(&data), out, "length {n}");
        }
    }

    #[test]
    fn any_split_gives_the_same_digest() {
        for &(n, inp, out) in VECTORS {
            let data = input(inp);
            for block in [1usize, 2, 7, 11, 64, 159, 160, 161, 500] {
                let mut h = QuickXor::new();
                for c in data.chunks(block) {
                    h.update(c);
                }
                assert_eq!(h.finish_base64(), out, "length {n}, blocks of {block}");
            }
        }
    }

    #[test]
    fn files_and_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert_eq!(file_quickxor(&p).unwrap(), quickxor_base64(&data));
        assert!(same_hash(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            " AAAAAAAAAAAAAAAAAAAAAAAAAAA= "
        ));
        assert!(!same_hash(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "SgAAAAAAAAAAAAAAAQAAAAAAAAA="
        ));
    }
}
