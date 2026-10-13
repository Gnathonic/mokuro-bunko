//! Uploads (new; mega-rs POSTed the whole file once, so a dropped connection meant
//! starting over).
//!
//! An upload is `u` (a URL for `size` bytes), then POSTs of ciphertext to
//! `<url>/<offset>`, each starting and ending on a MAC chunk boundary, then `p` with
//! the completion token the last POST answers. [`UploadState`] holds everything a
//! later process needs to continue: the URL, the file key and nonce, the bytes MEGA
//! has acknowledged and their chunk MACs. It contains the file key: persist it like a
//! credential.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use bytes::Bytes;
use cipher::StreamCipher;
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::crypto::{
    self, Fingerprint, MacStream, NodeAttributes, NodeKey, b64, chunk_at, ctr_at, unb64,
};
use crate::error::{Error, ErrorCode, Result};
use crate::protocol::{FileNode, NewNode, NodeKind, PutNodesResponse, Request, UploadResponse};

/// Default largest POST (several MAC chunks).
pub const DEFAULT_POST_MAX: u64 = 8 << 20;

/// A resumable upload.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadState {
    pub url: String,
    pub size: u64,
    key: String,
    nonce: String,
    /// Bytes MEGA has acknowledged (a chunk boundary, or `size`).
    pub offset: u64,
    /// Chunk MACs of the acknowledged bytes, base64.
    macs: Vec<String>,
    /// The completion token, once the last POST answered it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl std::fmt::Debug for UploadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadState")
            .field("size", &self.size)
            .field("offset", &self.offset)
            .field("done", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl UploadState {
    fn key(&self) -> Result<([u8; 16], [u8; 8])> {
        let k: [u8; 16] = unb64(&self.key)?
            .try_into()
            .map_err(|_| Error::Protocol("upload key".into()))?;
        let n: [u8; 8] = unb64(&self.nonce)?
            .try_into()
            .map_err(|_| Error::Protocol("upload nonce".into()))?;
        Ok((k, n))
    }

    fn macs(&self) -> Result<Vec<[u8; 16]>> {
        self.macs
            .iter()
            .map(|m| {
                unb64(m)?
                    .try_into()
                    .map_err(|_| Error::Protocol("chunk MAC".into()))
            })
            .collect()
    }

    /// Whether the state is internally consistent (else restart the upload).
    pub fn is_consistent(&self) -> bool {
        let Ok(macs) = self.macs() else {
            return false;
        };
        if self.key().is_err() || self.offset > self.size {
            return false;
        }
        // One MAC per acknowledged chunk.
        let mut n = 0usize;
        let mut off = 0u64;
        while off < self.offset {
            let (s, l) = chunk_at(off);
            off = (s + l).min(self.size);
            n += 1;
        }
        off == self.offset && n == macs.len()
    }

    /// The node key, once every byte is acknowledged.
    pub fn node_key(&self) -> Result<NodeKey> {
        if self.offset != self.size {
            return Err(Error::Protocol("upload not finished".into()));
        }
        let (aes, nonce) = self.key()?;
        let cipher = <aes::Aes128 as cipher::KeyInit>::new(&aes.into());
        let mac = crypto::condense(&cipher, &self.macs()?);
        Ok(NodeKey::File { aes, nonce, mac })
    }
}

/// What one chunk POST answered.
enum Posted {
    More,
    Token(String),
}

fn token_of(body: &[u8]) -> Result<Posted> {
    if body.is_empty() {
        return Ok(Posted::More);
    }
    if let Ok(s) = std::str::from_utf8(body) {
        let s = s.trim();
        if let Ok(n) = s.parse::<i64>()
            && n < 0
        {
            return Err(Error::Api(ErrorCode::from_i64(n)));
        }
        if !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Ok(Posted::Token(s.to_string()));
        }
    }
    // A binary token (the `v: 2` form megajs requests): base64 it.
    Ok(Posted::Token(b64(body)))
}

impl Client {
    /// Start an upload of `size` bytes with a fresh key.
    pub async fn upload_begin(&self, size: u64) -> Result<UploadState> {
        let v = self
            .cs1(Request::Upload {
                s: size,
                ssl: self.ssl(),
            })
            .await?;
        let r: UploadResponse = serde_json::from_value(v)?;
        let key: [u8; 16] = rand::random();
        let nonce: [u8; 8] = rand::random();
        Ok(UploadState {
            url: r.url,
            size,
            key: b64(&key),
            nonce: b64(&nonce),
            offset: 0,
            macs: Vec::new(),
            token: None,
        })
    }

    /// Send the rest of `path` (from `state.offset`) and return the completion token.
    /// `progress` sees the state after every acknowledged POST: persist it there.
    /// On error the state still describes what MEGA has.
    pub async fn upload_send(
        &self,
        state: &mut UploadState,
        path: &Path,
        post_max: u64,
        progress: &mut (dyn FnMut(&UploadState) + Send),
    ) -> Result<String> {
        if let Some(t) = &state.token {
            return Ok(t.clone());
        }
        if !state.is_consistent() {
            return Err(Error::Protocol("inconsistent upload state".into()));
        }
        let (aes, nonce) = state.key()?;
        let _permit = self.transfer_permit().await;
        let size = state.size;
        if size == 0 {
            let url = format!("{}/0", state.url.trim_end_matches('/'));
            return match self.post_chunk(&url, Bytes::new()).await? {
                Posted::Token(t) => {
                    state.token = Some(t.clone());
                    progress(state);
                    Ok(t)
                }
                Posted::More => Err(Error::Protocol("empty upload without a token".into())),
            };
        }
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len != size {
            return Err(Error::Protocol(format!(
                "the file is {len} bytes, the upload {size}"
            )));
        }
        let post_max = post_max.max(128 << 10);
        while state.offset < size {
            let start = state.offset;
            // Whole MAC chunks up to post_max (at least one).
            let mut end = start;
            loop {
                let (s, l) = chunk_at(end);
                let next = (s + l).min(size);
                if end > start && next - start > post_max {
                    break;
                }
                end = next;
                if end == size {
                    break;
                }
            }
            let mut plain = vec![0u8; (end - start) as usize];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut plain)?;
            let mut mac = MacStream::resume(&aes, &nonce, start, Vec::new());
            mac.update(&plain);
            let new_macs = mac.finish_macs();
            ctr_at(&aes, &nonce, start).apply_keystream(&mut plain);
            let url = format!("{}/{start}", state.url.trim_end_matches('/'));
            let posted = self.post_chunk(&url, Bytes::from(plain)).await?;
            state.offset = end;
            state.macs.extend(new_macs.iter().map(|m| b64(m)));
            match posted {
                Posted::Token(t) if end == size => {
                    state.token = Some(t.clone());
                    progress(state);
                    return Ok(t);
                }
                Posted::Token(t) => {
                    return Err(Error::Protocol(format!(
                        "completion token {t:?} before the last chunk"
                    )));
                }
                Posted::More if end == size => {
                    return Err(Error::Protocol("last chunk answered no token".into()));
                }
                Posted::More => progress(state),
            }
        }
        Err(Error::Protocol("upload ended without a token".into()))
    }

    async fn post_chunk(&self, url: &str, body: Bytes) -> Result<Posted> {
        let len = body.len();
        let resp = self
            .http()
            .post(url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Http {
                status: status.as_u16(),
                retry_after: None,
            });
        }
        token_of(&resp.bytes().await?)
    }

    /// Create the uploaded file `name` under `parent` (`p`). `idem` makes a retried
    /// completion recognisable in the feed.
    pub async fn upload_complete(
        &self,
        parent: &str,
        name: &str,
        state: &UploadState,
        fingerprint: Fingerprint,
        idem: &str,
    ) -> Result<FileNode> {
        let session = self.session_or_err()?;
        let token = state
            .token
            .clone()
            .ok_or_else(|| Error::Protocol("upload not finished".into()))?;
        let key = state.node_key()?;
        let attrs = NodeAttributes {
            name: name.to_string(),
            fingerprint: Some(fingerprint.serialize()),
            ..Default::default()
        };
        let a = b64(&attrs.encrypt(key.aes())?);
        let k = session.wrap_key(&key)?;
        self.put_node(
            parent,
            NewNode {
                h: token,
                t: NodeKind::File,
                a,
                k,
            },
            idem,
        )
        .await
    }

    /// Create the folder `name` under `parent`.
    pub async fn create_folder(&self, parent: &str, name: &str, idem: &str) -> Result<FileNode> {
        let session = self.session_or_err()?;
        let key = NodeKey::Folder {
            aes: rand::random(),
        };
        let attrs = NodeAttributes {
            name: name.to_string(),
            ..Default::default()
        };
        let a = b64(&attrs.encrypt(key.aes())?);
        let k = session.wrap_key(&key)?;
        self.put_node(
            parent,
            NewNode {
                h: "xxxxxxxx".into(),
                t: NodeKind::Folder,
                a,
                k,
            },
            idem,
        )
        .await
    }

    async fn put_node(&self, parent: &str, node: NewNode, idem: &str) -> Result<FileNode> {
        let v = self
            .cs1(Request::PutNodes {
                t: parent.to_string(),
                n: vec![node],
                i: idem.to_string(),
            })
            .await?;
        let r: PutNodesResponse = serde_json::from_value(v)?;
        r.nodes
            .into_iter()
            .next()
            .ok_or_else(|| Error::Protocol("p answered no node".into()))
    }

    /// Rename: re-encrypt `attrs` (all of the node's fields, with the new name).
    pub async fn set_attributes(
        &self,
        handle: &str,
        key: &NodeKey,
        attrs: &NodeAttributes,
        idem: &str,
    ) -> Result<()> {
        let attr = b64(&attrs.encrypt(key.aes())?);
        self.cs1(Request::SetAttributes {
            n: handle.to_string(),
            attr,
            i: idem.to_string(),
        })
        .await
        .map(|_| ())
    }

    /// Move `handle` under `parent` (the rubbish bin for a trash).
    pub async fn move_node(&self, handle: &str, parent: &str, idem: &str) -> Result<()> {
        self.cs1(Request::Move {
            n: handle.to_string(),
            t: parent.to_string(),
            i: idem.to_string(),
        })
        .await
        .map(|_| ())
    }

    /// Delete `handle` and its subtree permanently.
    pub async fn delete_node(&self, handle: &str, idem: &str) -> Result<()> {
        self.cs1(Request::Delete {
            n: handle.to_string(),
            i: idem.to_string(),
        })
        .await
        .map(|_| ())
    }
}

/// The fingerprint of a local file (sparse checksum + `mtime`, unix seconds).
pub fn file_fingerprint(path: &Path, mtime: i64) -> Result<Fingerprint> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let checksum = crypto::sparse_checksum(&mut f, size)?;
    Ok(Fingerprint {
        checksum,
        modified_at: mtime,
    })
}

/// A random idempotence id for `p`/`a`/`m`/`d` (`i`).
pub fn idempotence_id() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    Alphanumeric.sample_string(&mut rand::rng(), 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_errors() {
        assert!(matches!(token_of(b"").unwrap(), Posted::More));
        assert!(matches!(
            token_of(b"-3"),
            Err(Error::Api(ErrorCode::EAGAIN))
        ));
        assert!(matches!(
            token_of(b"-17"),
            Err(Error::Api(ErrorCode::EOVERQUOTA))
        ));
        let Posted::Token(t) = token_of(b"AbC-_9").unwrap() else {
            panic!()
        };
        assert_eq!(t, "AbC-_9");
        let Posted::Token(t) = token_of(&[0xff, 0x00, 0x10]).unwrap() else {
            panic!()
        };
        assert_eq!(t, b64(&[0xff, 0x00, 0x10]));
    }

    #[test]
    fn state_consistency() {
        let mut s = UploadState {
            url: "u".into(),
            size: 300 << 10,
            key: b64(&[1; 16]),
            nonce: b64(&[2; 8]),
            offset: 0,
            macs: vec![],
            token: None,
        };
        assert!(s.is_consistent());
        s.offset = 128 << 10;
        assert!(!s.is_consistent());
        s.macs.push(b64(&[0; 16]));
        assert!(s.is_consistent());
        s.offset = 129 << 10;
        assert!(!s.is_consistent());
        s.offset = 300 << 10;
        s.macs.push(b64(&[0; 16]));
        assert!(s.is_consistent());
        assert!(s.node_key().is_ok());
        // Debug does not print the key.
        assert!(!format!("{s:?}").contains(&b64(&[1; 16])));
    }
}
