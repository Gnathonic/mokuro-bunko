//! MEGA's cryptography.
//!
//! Vendored from mega-rs (`utils/mod.rs`, `utils/rsa.rs`, `attributes.rs`,
//! `fingerprint.rs`): the v1/v2 key derivation, ECB key wrapping, key/MAC merging,
//! attribute blocks, the fingerprint, the sparse checksum, the condensed MAC and the
//! MPI/RSA helpers. Changes: no panics on malformed input, synchronous
//! sparse checksum over `Read + Seek` (and the tiny-file case reads `size` bytes, not
//! 16), the condensed MAC split into per-chunk MACs ([`MacStream`]) so an upload can
//! resume, and [`ctr_at`] for decrypting from an arbitrary offset.

use std::io::{Read, Seek, SeekFrom};

use aes::Aes128;
use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, general_purpose};
use cipher::{
    BlockDecrypt, BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher,
};
use serde::{Deserialize, Serialize};
use sha2::Sha512;

use crate::error::{Error, Result};

/// AES-128-CTR with a 128-bit big-endian counter, as MEGA uses for file contents.
pub type Ctr = ctr::Ctr128BE<Aes128>;

/// Size of a file node key (aes key ^ (nonce || mac), then nonce, then mac).
pub const FILE_KEY_SIZE: usize = 32;
/// Size of a folder node key.
pub const FOLDER_KEY_SIZE: usize = 16;

const B64_DECODE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// MEGA's base64: URL-safe, unpadded.
pub fn b64(data: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Decode MEGA base64 (also accepts `+`, `/` and padding, as megajs does).
pub fn unb64(s: &str) -> Result<Vec<u8>> {
    let s: String = s
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    Ok(B64_DECODE.decode(s.as_bytes())?)
}

/// The v1 password key (`prepare_key`): 65536 rounds of AES over a constant, keyed by
/// each 16-byte block of the password.
pub fn prepare_key_v1(password: &[u8]) -> [u8; 16] {
    let mut data = aes::Block::from([
        0x93u8, 0xC4, 0x67, 0xE3, 0x7D, 0xB0, 0xC7, 0xA4, 0xD1, 0xBE, 0x3F, 0x81, 0x01, 0x52, 0xCB,
        0x56,
    ]);
    let ciphers: Vec<Aes128> = password
        .chunks(16)
        .map(|chunk| {
            let mut key = [0u8; 16];
            key[..chunk.len()].copy_from_slice(chunk);
            Aes128::new(&key.into())
        })
        .collect();
    for _ in 0..65536 {
        for aes in &ciphers {
            aes.encrypt_block(&mut data);
        }
    }
    data.into()
}

/// The v1 login hash of the (lower-cased) email under the v1 password key, as
/// base64 (megajs `stringhash`).
pub fn user_hash_v1(key: &[u8; 16], email: &[u8]) -> [u8; 8] {
    let mut hash = aes::Block::from([0u8; 16]);
    for (i, b) in email.iter().enumerate() {
        hash[i % 16] ^= b;
    }
    let aes = Aes128::new(key.into());
    for _ in 0..16384 {
        aes.encrypt_block(&mut hash);
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&hash[0..4]);
    out[4..].copy_from_slice(&hash[8..12]);
    out
}

/// The v2 derivation: PBKDF2-HMAC-SHA512, 100,000 rounds, 32 bytes. The first half is
/// the password key, the second the login hash.
pub fn prepare_key_v2(password: &[u8], salt: &[u8]) -> [u8; 32] {
    pbkdf2::pbkdf2_hmac_array::<Sha512, 32>(password, salt, 100_000)
}

fn check_blocks(data: &[u8]) -> Result<()> {
    if data.len().is_multiple_of(16) {
        Ok(())
    } else {
        Err(Error::Crypto(format!(
            "{} bytes is not a whole number of AES blocks",
            data.len()
        )))
    }
}

/// AES-128-ECB encryption in place (key wrapping).
pub fn ecb_encrypt(key: &[u8; 16], data: &mut [u8]) -> Result<()> {
    check_blocks(data)?;
    let aes = Aes128::new(key.into());
    for block in data.as_chunks_mut::<16>().0 {
        aes.encrypt_block(aes::Block::from_mut_slice(block));
    }
    Ok(())
}

/// AES-128-ECB decryption in place (key unwrapping).
pub fn ecb_decrypt(key: &[u8; 16], data: &mut [u8]) -> Result<()> {
    check_blocks(data)?;
    let aes = Aes128::new(key.into());
    for block in data.as_chunks_mut::<16>().0 {
        aes.decrypt_block(aes::Block::from_mut_slice(block));
    }
    Ok(())
}

/// A file key as stored: `k[0..16] ^= k[16..32]` (merge and unmerge are the same XOR).
pub fn merge_key_mac(key: &mut [u8; FILE_KEY_SIZE]) {
    let (fst, snd) = key.split_at_mut(16);
    for (a, b) in fst.iter_mut().zip(snd.iter()) {
        *a ^= *b;
    }
}

/// A decrypted node key.
#[derive(Clone, PartialEq, Eq, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub enum NodeKey {
    File {
        aes: [u8; 16],
        nonce: [u8; 8],
        /// The condensed MAC of the plaintext.
        mac: [u8; 8],
    },
    Folder {
        aes: [u8; 16],
    },
}

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeKey::File { .. } => f.write_str("NodeKey::File(..)"),
            NodeKey::Folder { .. } => f.write_str("NodeKey::Folder(..)"),
        }
    }
}

impl NodeKey {
    /// From the unwrapped (ECB-decrypted) key bytes: 32 for a file, 16 for a folder.
    pub fn from_unwrapped(is_file: bool, raw: &[u8]) -> Option<NodeKey> {
        if is_file {
            let mut k: [u8; FILE_KEY_SIZE] = raw.get(..FILE_KEY_SIZE)?.try_into().ok()?;
            merge_key_mac(&mut k);
            let mut aes = [0u8; 16];
            let mut nonce = [0u8; 8];
            let mut mac = [0u8; 8];
            aes.copy_from_slice(&k[..16]);
            nonce.copy_from_slice(&k[16..24]);
            mac.copy_from_slice(&k[24..32]);
            Some(NodeKey::File { aes, nonce, mac })
        } else {
            let aes: [u8; 16] = raw.get(..FOLDER_KEY_SIZE)?.try_into().ok()?;
            Some(NodeKey::Folder { aes })
        }
    }

    /// The key bytes to wrap and store (`k` of a `p` command).
    pub fn packed(&self) -> Vec<u8> {
        match self {
            NodeKey::File { aes, nonce, mac } => {
                let mut k = [0u8; FILE_KEY_SIZE];
                k[..16].copy_from_slice(aes);
                k[16..24].copy_from_slice(nonce);
                k[24..].copy_from_slice(mac);
                merge_key_mac(&mut k);
                k.to_vec()
            }
            NodeKey::Folder { aes } => aes.to_vec(),
        }
    }

    pub fn aes(&self) -> &[u8; 16] {
        match self {
            NodeKey::File { aes, .. } | NodeKey::Folder { aes } => aes,
        }
    }
}

/// A node's attribute block: `MEGA{json}` NUL-padded, AES-CBC with a zero IV.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodeAttributes {
    #[serde(rename = "n", default)]
    pub name: String,
    /// The encoded fingerprint (`c`).
    #[serde(rename = "c", default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// A last-modified time some clients write (`t`).
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
    /// Every other field, kept so a rename does not drop it.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl NodeAttributes {
    pub fn decrypt(key: &[u8; 16], mut buffer: Vec<u8>) -> Result<Self> {
        check_blocks(&buffer)?;
        let mut cbc = cbc::Decryptor::<Aes128>::new(key.into(), &Default::default());
        for chunk in buffer.as_chunks_mut::<16>().0 {
            cbc.decrypt_block_mut(aes::Block::from_mut_slice(chunk));
        }
        if !buffer.starts_with(b"MEGA") {
            return Err(Error::Crypto("attributes do not start with MEGA".into()));
        }
        let len = buffer.iter().take_while(|b| **b != 0).count();
        serde_json::from_slice(&buffer[4..len])
            .map_err(|e| Error::Crypto(format!("attributes: {}", crate::error::json_err(&e))))
    }

    /// Decrypt the base64 `a`/`at` field.
    pub fn decrypt_b64(key: &[u8; 16], attr: &str) -> Result<Self> {
        Self::decrypt(key, unb64(attr)?)
    }

    pub fn encrypt(&self, key: &[u8; 16]) -> Result<Vec<u8>> {
        let mut buffer = b"MEGA".to_vec();
        serde_json::to_writer(&mut buffer, self)?;
        let padding = (16 - buffer.len() % 16) % 16;
        buffer.resize(buffer.len() + padding, 0);
        let mut cbc = cbc::Encryptor::<Aes128>::new(key.into(), &Default::default());
        for chunk in buffer.as_chunks_mut::<16>().0 {
            cbc.encrypt_block_mut(aes::Block::from_mut_slice(chunk));
        }
        Ok(buffer)
    }

    pub fn fingerprint(&self) -> Option<Fingerprint> {
        Fingerprint::deserialize(self.fingerprint.as_deref()?)
    }
}

/// The node fingerprint: the sparse checksum and the client's mtime (unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub checksum: [u8; 16],
    pub modified_at: i64,
}

impl Fingerprint {
    pub fn deserialize(s: &str) -> Option<Self> {
        let decoded = unb64(s).ok()?;
        if decoded.len() < 17 {
            return None;
        }
        let (checksum, mtime) = decoded.split_at(16);
        let (count, mtime) = mtime.split_first()?;
        let count = usize::from(*count);
        if count > 8 || count > mtime.len() {
            return None;
        }
        let modified_at = mtime[..count]
            .iter()
            .rev()
            .fold(0i64, |acc, b| (acc << 8) + i64::from(*b));
        Some(Fingerprint {
            checksum: checksum.try_into().ok()?,
            modified_at,
        })
    }

    pub fn serialize(&self) -> String {
        let mut buffer = Vec::with_capacity(25);
        buffer.extend_from_slice(&self.checksum);
        let mut tail = Vec::with_capacity(8);
        let mut value = self.modified_at.max(0);
        while value > 0 {
            tail.push((value & 0xFF) as u8);
            value >>= 8;
        }
        buffer.push(tail.len() as u8);
        buffer.extend_from_slice(&tail);
        b64(&buffer)
    }
}

/// MEGA's sparse CRC32 checksum of `size` bytes from the reader's current position
/// (full coverage up to 8 KiB, sampled above).
pub fn sparse_checksum<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<[u8; 16]> {
    const MAXFULL: u64 = 8192;
    const BLOCK_SIZE: u64 = 64;
    let start = r.stream_position()?;
    let mut checksum = [0u8; 16];
    if size <= 16 {
        r.read_exact(&mut checksum[..size as usize])?;
        return Ok(checksum);
    }
    if size <= MAXFULL {
        let mut buffer = vec![0u8; size as usize];
        r.read_exact(&mut buffer)?;
        let size = size as usize;
        for i in 0..4 {
            let crc = crc32fast::hash(&buffer[i * size / 4..(i + 1) * size / 4]);
            checksum[i * 4..i * 4 + 4].copy_from_slice(&crc.to_be_bytes());
        }
        return Ok(checksum);
    }
    let mut block = [0u8; BLOCK_SIZE as usize];
    let blocks = MAXFULL / (BLOCK_SIZE * 4);
    for idx in 0..4u64 {
        let mut hasher = crc32fast::Hasher::new();
        for blk in 0..blocks {
            let offset = (size - BLOCK_SIZE) * (idx * blocks + blk) / (4 * blocks - 1);
            r.seek(SeekFrom::Start(start + offset))?;
            r.read_exact(&mut block)?;
            hasher.update(&block);
        }
        let i = idx as usize * 4;
        checksum[i..i + 4].copy_from_slice(&hasher.finalize().to_be_bytes());
    }
    Ok(checksum)
}

/// The MAC chunk boundaries: 128 KiB, 256 KiB, … 1 MiB, then 1 MiB each.
/// Returns the chunk `(start, len)` that contains `offset`.
pub fn chunk_at(offset: u64) -> (u64, u64) {
    let mut start = 0u64;
    let mut len = 128 << 10;
    loop {
        if offset < start + len {
            return (start, len);
        }
        start += len;
        if len < 1 << 20 {
            len += 128 << 10;
        } else {
            // Past the growing prefix every chunk is 1 MiB: jump straight there.
            start += ((offset - start) >> 20) << 20;
        }
    }
}

/// Whether `offset` starts a MAC chunk (0 does).
pub fn is_chunk_boundary(offset: u64) -> bool {
    chunk_at(offset).0 == offset
}

/// One chunk's MAC: CBC-MAC with IV `nonce || nonce`, the last block zero-padded.
pub fn chunk_mac(aes: &Aes128, nonce: &[u8; 8], data: &[u8]) -> [u8; 16] {
    let mut cur = aes::Block::default();
    cur[..8].copy_from_slice(nonce);
    cur[8..].copy_from_slice(nonce);
    for block in data.chunks(16) {
        for (c, b) in cur.iter_mut().zip(block) {
            *c ^= b;
        }
        aes.encrypt_block(&mut cur);
    }
    cur.into()
}

/// Fold chunk MACs into the 8-byte condensed MAC stored in the node key.
pub fn condense(aes: &Aes128, macs: &[[u8; 16]]) -> [u8; 8] {
    let mut mac = aes::Block::default();
    for m in macs {
        for (a, b) in mac.iter_mut().zip(m) {
            *a ^= b;
        }
        aes.encrypt_block(&mut mac);
    }
    let mut out = [0u8; 8];
    for i in 0..4 {
        out[i] = mac[i] ^ mac[i + 4];
        out[i + 4] = mac[i + 8] ^ mac[i + 12];
    }
    out
}

/// The condensed MAC megajs computes: it also folds in the empty chunk that follows
/// a size on a chunk boundary (and the one empty chunk of an empty file). Verification
/// accepts it beside the SDK's form so files the reader uploaded at those sizes pass.
pub fn condense_megajs(aes: &Aes128, nonce: &[u8; 8], macs: &[[u8; 16]]) -> [u8; 8] {
    let mut all = macs.to_vec();
    let mut empty = [0u8; 16];
    empty[..8].copy_from_slice(nonce);
    empty[8..].copy_from_slice(nonce);
    all.push(empty);
    condense(aes, &all)
}

/// Incremental chunk MACs over a plaintext stream that starts at `offset` (a chunk
/// boundary).
pub struct MacStream {
    aes: Aes128,
    nonce: [u8; 8],
    pos: u64,
    chunk_end: u64,
    cur: aes::Block,
    partial: Vec<u8>,
    /// The MACs of the completed chunks.
    pub macs: Vec<[u8; 16]>,
}

impl MacStream {
    pub fn new(key: &[u8; 16], nonce: &[u8; 8]) -> Self {
        Self::resume(key, nonce, 0, Vec::new())
    }

    /// Continue after `offset` bytes whose chunk MACs are `macs`.
    pub fn resume(key: &[u8; 16], nonce: &[u8; 8], offset: u64, macs: Vec<[u8; 16]>) -> Self {
        let (start, len) = chunk_at(offset);
        debug_assert_eq!(start, offset, "MacStream resumed off a chunk boundary");
        let mut s = MacStream {
            aes: Aes128::new(key.into()),
            nonce: *nonce,
            pos: offset,
            chunk_end: start + len,
            cur: aes::Block::default(),
            partial: Vec::with_capacity(16),
            macs,
        };
        s.reset_cur();
        s
    }

    fn reset_cur(&mut self) {
        self.cur[..8].copy_from_slice(&self.nonce);
        self.cur[8..].copy_from_slice(&self.nonce);
    }

    fn block(&mut self, b: &[u8]) {
        for (c, x) in self.cur.iter_mut().zip(b) {
            *c ^= x;
        }
        self.aes.encrypt_block(&mut self.cur);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let room = (self.chunk_end - self.pos) as usize;
            let take = room.min(data.len());
            let (now, rest) = data.split_at(take);
            data = rest;
            let mut now = now;
            if !self.partial.is_empty() {
                let need = 16 - self.partial.len();
                let n = need.min(now.len());
                self.partial.extend_from_slice(&now[..n]);
                now = &now[n..];
                if self.partial.len() == 16 {
                    let mut b = [0u8; 16];
                    b.copy_from_slice(&self.partial);
                    self.block(&b);
                    self.partial.clear();
                }
            }
            let whole = now.len() - now.len() % 16;
            for b in now[..whole].as_chunks::<16>().0 {
                self.block(b);
            }
            self.partial.extend_from_slice(&now[whole..]);
            self.pos += take as u64;
            if self.pos == self.chunk_end {
                self.end_chunk();
            }
        }
    }

    fn end_chunk(&mut self) {
        if !self.partial.is_empty() {
            let mut b = [0u8; 16];
            b[..self.partial.len()].copy_from_slice(&self.partial);
            self.block(&b);
            self.partial.clear();
        }
        self.macs.push(self.cur.into());
        self.reset_cur();
        let (start, len) = chunk_at(self.pos);
        self.chunk_end = start + len;
    }

    /// Bytes seen so far (including the resumed prefix).
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Close the last (partial) chunk; the chunk MACs of the whole stream.
    pub fn finish_macs(mut self) -> Vec<[u8; 16]> {
        let (start, _) = chunk_at(self.pos);
        if self.pos > start || !self.partial.is_empty() {
            self.end_chunk();
        }
        std::mem::take(&mut self.macs)
    }

    /// Close the stream and condense (the SDK's form).
    pub fn finish(self) -> [u8; 8] {
        let aes = self.aes.clone();
        condense(&aes, &self.finish_macs())
    }

    /// Close the stream; true when the condensed MAC matches either the SDK's form or
    /// megajs's.
    pub fn verify(self, expected: &[u8; 8]) -> bool {
        let aes = self.aes.clone();
        let nonce = self.nonce;
        let macs = self.finish_macs();
        condense(&aes, &macs) == *expected || condense_megajs(&aes, &nonce, &macs) == *expected
    }
}

/// An AES-128-CTR keystream positioned at byte `offset` of the file: the counter
/// block is `nonce || (offset / 16)` big-endian, and the first `offset % 16` keystream
/// bytes are consumed.
pub fn ctr_at(key: &[u8; 16], nonce: &[u8; 8], offset: u64) -> Ctr {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(nonce);
    iv[8..].copy_from_slice(&(offset / 16).to_be_bytes());
    let mut ctr = Ctr::new(key.into(), &iv.into());
    let skip = (offset % 16) as usize;
    if skip > 0 {
        let mut junk = [0u8; 16];
        ctr.apply_keystream(&mut junk[..skip]);
    }
    ctr
}

/// The MPI-encoded RSA private key (`privk`): p, q, d, u.
#[derive(Clone, Default, zeroize::Zeroize)]
pub struct RsaPrivateKey {
    p: rsa::BigUint,
    q: rsa::BigUint,
    d: rsa::BigUint,
}

impl std::fmt::Debug for RsaPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsaPrivateKey(..)")
    }
}

impl RsaPrivateKey {
    pub fn from_mpi_bytes(data: &[u8]) -> Result<Self> {
        let (p, data) = get_mpi(data)?;
        let (q, data) = get_mpi(data)?;
        let (d, data) = get_mpi(data)?;
        let _u = get_mpi(data)?;
        Ok(Self {
            p: rsa::BigUint::from_bytes_be(p),
            q: rsa::BigUint::from_bytes_be(q),
            d: rsa::BigUint::from_bytes_be(d),
        })
    }

    /// Raw RSA: `m^d mod pq` (not constant-time; see the crate's Cargo.toml note).
    pub fn decrypt(&self, data: &[u8]) -> Vec<u8> {
        let m = rsa::BigUint::from_bytes_be(data);
        let n = &self.p * &self.q;
        m.modpow(&self.d, &n).to_bytes_be()
    }
}

/// Split one MPI (2-byte bit length, then the big-endian bytes) off `data`.
pub fn get_mpi(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let &[fst, snd, ref data @ ..] = data else {
        return Err(Error::Crypto("truncated MPI".into()));
    };
    let len = (usize::from(fst) * 256 + usize::from(snd)).div_ceil(8);
    if len > data.len() {
        return Err(Error::Crypto("truncated MPI".into()));
    }
    Ok(data.split_at(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};

    /// megajs `testBuffer`.
    fn test_buffer(size: usize, start: usize, step: usize) -> Vec<u8> {
        (0..size)
            .map(|i| ((start + i * step) % 255) as u8)
            .collect()
    }

    // mega-rs utils tests == megajs prepareKey.test.mjs.
    #[test]
    fn prepare_key_v1_vectors() {
        let k = |n| hex::encode(prepare_key_v1(&test_buffer(n, 0, 1)));
        assert_eq!(k(8), "c4589a459956887caf0b408635c3c03b");
        assert_eq!(k(10), "59930b1c55d783ac77df4c4ff261b0f1");
        assert_eq!(k(64), "83bd84689f057f9ed9834b3ecb81d80e");
    }

    // megajs stringhash.test.mjs.
    #[test]
    fn user_hash_v1_vectors() {
        let key = prepare_key_v1(&test_buffer(8, 0, 1));
        let h = |n| hex::encode(user_hash_v1(&key, &test_buffer(n, 0, 1)));
        assert_eq!(h(10), "9e791646c66840b5");
        assert_eq!(h(16), "6ba07aca224e84a4");
        assert_eq!(h(32), "6a1e6c5539c0ed48");
    }

    // Produced with Node's crypto.pbkdf2Sync (what megajs prepareKeyV2 calls).
    #[test]
    fn prepare_key_v2_vector() {
        let salt: Vec<u8> = (0..16).collect();
        assert_eq!(
            hex::encode(prepare_key_v2(b"correct horse battery staple", &salt)),
            "8736985eadc89cfee314d74a15389705a28c73a1e48ba151f1fc29f25442352c"
        );
    }

    // megajs aes-ctr.test.mjs "AES-CTR small" and "large".
    #[test]
    fn ctr_vectors() {
        let key = test_buffer(24, 0, 1);
        let aes: [u8; 16] = key[..16].try_into().unwrap();
        let nonce: [u8; 8] = key[16..24].try_into().unwrap();
        let mut data = test_buffer(32, 0, 1);
        ctr_at(&aes, &nonce, 0).apply_keystream(&mut data);
        assert_eq!(
            hex::encode(&data),
            "8de7dac3d95eca9fd74f30c1ecf8247a8f25d1b3fd2d11a8a7b458d16a085434"
        );
        let mut big = test_buffer(151_511, 0, 1);
        ctr_at(&aes, &nonce, 0).apply_keystream(&mut big);
        assert_eq!(
            hex::encode(Sha1::digest(&big)),
            "2ec4f058ba6100f7e28b4ff7bb9a711a5de57c64"
        );
    }

    #[test]
    fn ctr_at_offset_equals_full_slice() {
        let aes = [7u8; 16];
        let nonce = [9u8; 8];
        let plain = test_buffer(300_007, 3, 7);
        let mut full = plain.clone();
        ctr_at(&aes, &nonce, 0).apply_keystream(&mut full);
        for off in [
            0u64, 1, 15, 16, 17, 31, 4095, 131_071, 131_072, 131_073, 299_999,
        ] {
            let mut part = full[off as usize..].to_vec();
            ctr_at(&aes, &nonce, off).apply_keystream(&mut part);
            assert_eq!(part, &plain[off as usize..], "offset {off}");
        }
        // The counter crosses a 2^8 boundary the way megajs's incrementCTRBuffer does.
        let off = 16 * 300;
        let mut a = full[off..off + 64].to_vec();
        ctr_at(&aes, &nonce, off as u64).apply_keystream(&mut a);
        assert_eq!(a, &plain[off..off + 64]);
    }

    // megajs crypto-stream.test.mjs: encrypting testBuffer(151511) with key
    // testBuffer(24, 100, 7) yields this stored (merged) key and ciphertext.
    #[test]
    fn upload_key_and_condensed_mac_vector() {
        let key = test_buffer(24, 100, 7);
        let aes: [u8; 16] = key[..16].try_into().unwrap();
        let nonce: [u8; 8] = key[16..].try_into().unwrap();
        let plain = test_buffer(151_511, 0, 1);
        let mut mac = MacStream::new(&aes, &nonce);
        // Uneven pieces, like the test's three writes.
        mac.update(&plain[..50_000]);
        mac.update(&plain[50_000..100_000]);
        mac.update(&plain[100_000..]);
        let condensed = mac.finish();
        let k = NodeKey::File {
            aes,
            nonce,
            mac: condensed,
        };
        assert_eq!(
            hex::encode(k.packed()),
            "b0b0909070707093e957d163217c2f3fd4dbe2e9f0f7fe0675f47bd299c3e9f2"
        );
        let mut ct = plain.clone();
        ctr_at(&aes, &nonce, 0).apply_keystream(&mut ct);
        assert_eq!(
            hex::encode(Sha1::digest(&ct)),
            "addb96c07ac4e6b66316b81530256c911b0b49d1"
        );
        // And the stored key round-trips.
        assert_eq!(NodeKey::from_unwrapped(true, &k.packed()), Some(k));
    }

    // megajs verify.test.mjs.
    #[test]
    fn verify_vector() {
        let key = unb64("AAAAAAAAAABnFCfbJFwAxwAAAAAAAAAAZxQn2yRcAMc").unwrap();
        let Some(NodeKey::File { aes, nonce, mac }) = NodeKey::from_unwrapped(true, &key) else {
            panic!("file key");
        };
        assert_eq!(hex::encode(mac), "671427db245c00c7");
        let mut m = MacStream::new(&aes, &nonce);
        m.update(&test_buffer(151_511, 0, 1));
        assert!(m.verify(&mac));
    }

    #[test]
    fn chunk_mac_matches_one_shot_and_resume() {
        let aes = [3u8; 16];
        let nonce = [4u8; 8];
        let cipher = Aes128::new(&aes.into());
        let data = test_buffer(5 << 20, 1, 3);
        // One shot, chunk by chunk.
        let mut macs = Vec::new();
        let mut off = 0u64;
        while off < data.len() as u64 {
            let (s, l) = chunk_at(off);
            assert_eq!(s, off);
            let end = (s + l).min(data.len() as u64);
            macs.push(chunk_mac(&cipher, &nonce, &data[s as usize..end as usize]));
            off = end;
        }
        let one_shot = condense(&cipher, &macs);
        // Streaming in odd pieces.
        let mut m = MacStream::new(&aes, &nonce);
        for piece in data.chunks(77_777) {
            m.update(piece);
        }
        assert_eq!(m.finish(), one_shot);
        // Resumed at a chunk boundary.
        let cut = 128 << 10 | 256 << 10; // 384 KiB = the third boundary
        assert!(is_chunk_boundary(cut));
        let mut first = MacStream::new(&aes, &nonce);
        first.update(&data[..cut as usize]);
        let done = first.finish_macs();
        assert_eq!(done.len(), 2);
        let mut rest = MacStream::resume(&aes, &nonce, cut, done);
        rest.update(&data[cut as usize..]);
        assert_eq!(rest.finish(), one_shot);
    }

    #[test]
    fn chunk_boundaries() {
        let mut bounds = Vec::new();
        let mut off = 0u64;
        for _ in 0..12 {
            let (s, l) = chunk_at(off);
            bounds.push((s >> 10, l >> 10));
            off = s + l;
        }
        assert_eq!(
            bounds,
            vec![
                (0, 128),
                (128, 256),
                (384, 384),
                (768, 512),
                (1280, 640),
                (1920, 768),
                (2688, 896),
                (3584, 1024),
                (4608, 1024),
                (5632, 1024),
                (6656, 1024),
                (7680, 1024),
            ]
        );
        assert_eq!(chunk_at(1 << 30).1, 1 << 20);
        assert!(is_chunk_boundary(4608 << 10));
        assert!(!is_chunk_boundary(4609 << 10));
        assert_eq!(chunk_at((200 << 20) + 5), chunk_at(200 << 20));
    }

    #[test]
    fn mac_on_boundaries_accepts_both_forms() {
        let aes = [1u8; 16];
        let nonce = [2u8; 8];
        let cipher = Aes128::new(&aes.into());
        for size in [0usize, 128 << 10] {
            let data = test_buffer(size, 0, 1);
            let mut m = MacStream::new(&aes, &nonce);
            m.update(&data);
            let macs = m.finish_macs();
            let sdk = condense(&cipher, &macs);
            let js = condense_megajs(&cipher, &nonce, &macs);
            assert_ne!(sdk, js);
            for want in [sdk, js] {
                let mut m = MacStream::new(&aes, &nonce);
                m.update(&data);
                assert!(m.verify(&want), "size {size}");
            }
        }
    }

    #[test]
    fn attributes_round_trip_and_keep_unknown_fields() {
        let key = [5u8; 16];
        let mut a = NodeAttributes {
            name: "Vol 1.cbz".into(),
            fingerprint: Some(
                Fingerprint {
                    checksum: [1; 16],
                    modified_at: 1_700_000_000,
                }
                .serialize(),
            ),
            ..Default::default()
        };
        a.other.insert("lbl".into(), serde_json::json!(3));
        let enc = a.encrypt(&key).unwrap();
        assert_eq!(enc.len() % 16, 0);
        let back = NodeAttributes::decrypt(&key, enc).unwrap();
        assert_eq!(back, a);
        assert_eq!(back.fingerprint().unwrap().modified_at, 1_700_000_000);
        // A wrong key is an error, not a panic.
        let enc = a.encrypt(&key).unwrap();
        assert!(NodeAttributes::decrypt(&[6u8; 16], enc).is_err());
        assert!(NodeAttributes::decrypt(&key, vec![0u8; 15]).is_err());
    }

    #[test]
    fn fingerprint_round_trip() {
        for t in [0i64, 1, 255, 256, 1_700_000_000, i64::from(u32::MAX) + 7] {
            let f = Fingerprint {
                checksum: [9; 16],
                modified_at: t,
            };
            assert_eq!(Fingerprint::deserialize(&f.serialize()), Some(f));
        }
        assert_eq!(Fingerprint::deserialize("AAAA"), None);
    }

    #[test]
    fn sparse_checksum_small_sizes() {
        let mut c = std::io::Cursor::new(b"abc".to_vec());
        let s = sparse_checksum(&mut c, 3).unwrap();
        assert_eq!(&s[..3], b"abc");
        assert_eq!(&s[3..], &[0u8; 13]);
        let data = test_buffer(5000, 0, 1);
        let s = sparse_checksum(&mut std::io::Cursor::new(&data), 5000).unwrap();
        assert_eq!(&s[..4], &crc32fast::hash(&data[..1250]).to_be_bytes());
        let big = test_buffer(1 << 20, 0, 3);
        let a = sparse_checksum(&mut std::io::Cursor::new(&big), 1 << 20).unwrap();
        let mut changed = big.clone();
        changed[0] ^= 1;
        let b = sparse_checksum(&mut std::io::Cursor::new(&changed), 1 << 20).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn mpi_and_ecb_errors() {
        assert!(get_mpi(&[0]).is_err());
        assert!(get_mpi(&[0, 16, 1]).is_err());
        assert_eq!(
            get_mpi(&[0, 9, 1, 2, 3]).unwrap(),
            (&[1u8, 2][..], &[3u8][..])
        );
        let mut odd = [0u8; 17];
        assert!(ecb_encrypt(&[0; 16], &mut odd).is_err());
        let mut k = [7u8; 32];
        ecb_encrypt(&[1; 16], &mut k).unwrap();
        ecb_decrypt(&[1; 16], &mut k).unwrap();
        assert_eq!(k, [7u8; 32]);
    }
}
