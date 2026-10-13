//! Downloads (new; mega-rs had only whole-file `download_node`).
//!
//! A read of bytes `first..=last` requests `<url>/<aligned>-<last>` with `aligned`
//! rounded down to the AES block, starts the CTR keystream at `nonce || aligned/16`
//! and drops `first - aligned` plaintext bytes. The status and the byte count are
//! checked before anything is trusted (an over-quota answer is a short error body,
//! mega-rs issue #16). A whole-file read also checks the condensed MAC; a ranged read
//! cannot be verified.

use std::pin::Pin;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use cipher::StreamCipher;
use futures_util::{Stream, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, RETRY_AFTER};

use crate::client::Client;
use crate::crypto::{Ctr, MacStream, NodeKey, ctr_at};
use crate::error::{Error, ErrorCode, Result};
use crate::protocol::{DownloadResponse, Request};

/// A decrypted body.
pub type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Where a file's ciphertext can be fetched (short-lived; fetch one per read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTarget {
    pub url: String,
    pub size: u64,
}

impl Client {
    /// A download URL for the file `handle` (`g`).
    pub async fn download_target(&self, handle: &str) -> Result<DownloadTarget> {
        let v = self
            .cs1(Request::Download {
                g: 1,
                ssl: self.ssl(),
                n: handle.to_string(),
            })
            .await?;
        let r: DownloadResponse = serde_json::from_value(v)?;
        let retry_after = r.time_left.filter(|t| *t > 0).map(Duration::from_secs);
        if let Some(e) = r.err.filter(|e| *e < 0) {
            return Err(match ErrorCode::from_i64(e) {
                ErrorCode::EOVERQUOTA => Error::OverQuota { retry_after },
                code => Error::Api(code),
            });
        }
        match r.url {
            Some(url) if !url.is_empty() => Ok(DownloadTarget { url, size: r.size }),
            _ if retry_after.is_some() => Err(Error::OverQuota { retry_after }),
            _ => Err(Error::Protocol("download answer without a URL".into())),
        }
    }

    /// Bytes `first..=last` (`last` `None`: to the end) of the file, decrypted.
    pub async fn read_range(
        &self,
        key: &NodeKey,
        target: &DownloadTarget,
        first: u64,
        last: Option<u64>,
    ) -> Result<ByteStream> {
        let NodeKey::File { aes, nonce, mac } = key else {
            return Err(Error::Protocol("not a file".into()));
        };
        let size = target.size;
        if size == 0 {
            if first > 0 {
                return Err(Error::Protocol(format!(
                    "range {first}- past the end of an empty file"
                )));
            }
            return Ok(Box::pin(futures_util::stream::empty()));
        }
        let last = last.unwrap_or(size - 1).min(size - 1);
        if first > last {
            return Err(Error::Protocol(format!(
                "range {first}-{last} not satisfiable (size {size})"
            )));
        }
        let aligned = first - first % 16;
        let expected = last - aligned + 1;
        let permit = self.transfer_permit().await;
        let url = format!("{}/{aligned}-{last}", target.url.trim_end_matches('/'));
        let resp = self.http().get(&url).send().await?;
        let status = resp.status();
        if status.as_u16() == 509 {
            let retry_after = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .map(Duration::from_secs);
            return Err(Error::OverQuota { retry_after });
        }
        if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
            return Err(Error::Http {
                status: status.as_u16(),
                retry_after: None,
            });
        }
        if let Some(len) = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            && len != expected
        {
            return Err(Error::ShortRead { expected, got: len });
        }
        let verify = (first == 0 && last == size - 1).then(|| (MacStream::new(aes, nonce), *mac));
        let st = ReadState {
            body: Box::pin(resp.bytes_stream()),
            ctr: ctr_at(aes, nonce, aligned),
            skip: first - aligned,
            expected,
            got: 0,
            verify,
            _permit: permit,
            done: false,
        };
        Ok(Box::pin(futures_util::stream::unfold(st, step)))
    }
}

type RawBody = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

struct ReadState {
    body: RawBody,
    ctr: Ctr,
    skip: u64,
    expected: u64,
    got: u64,
    verify: Option<(MacStream, [u8; 8])>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    done: bool,
}

fn io(e: Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

async fn step(mut st: ReadState) -> Option<(std::io::Result<Bytes>, ReadState)> {
    loop {
        if st.done {
            return None;
        }
        match st.body.next().await {
            Some(Err(e)) => {
                st.done = true;
                return Some((Err(std::io::Error::other(e)), st));
            }
            Some(Ok(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                st.got += chunk.len() as u64;
                if st.got > st.expected {
                    st.done = true;
                    let e = Error::ShortRead {
                        expected: st.expected,
                        got: st.got,
                    };
                    return Some((Err(io(e)), st));
                }
                let mut buf = BytesMut::from(&chunk[..]);
                st.ctr.apply_keystream(&mut buf);
                if let Some((m, _)) = st.verify.as_mut() {
                    m.update(&buf);
                }
                let mut out = buf.freeze();
                if st.skip > 0 {
                    let n = st.skip.min(out.len() as u64) as usize;
                    st.skip -= n as u64;
                    out = out.slice(n..);
                }
                if out.is_empty() {
                    continue;
                }
                return Some((Ok(out), st));
            }
            None => {
                st.done = true;
                if st.got != st.expected {
                    let e = Error::ShortRead {
                        expected: st.expected,
                        got: st.got,
                    };
                    return Some((Err(io(e)), st));
                }
                if let Some((m, want)) = st.verify.take()
                    && !m.verify(&want)
                {
                    return Some((Err(io(Error::MacMismatch)), st));
                }
                return None;
            }
        }
    }
}
