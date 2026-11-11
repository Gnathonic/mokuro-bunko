//! A MEGA API client for mokuro-bunko, derived from mega-rs 0.8.0 (see NOTICE).
//!
//! - [`Client::login`] (email, password, optional 2FA) and [`Client::resume`] from a
//!   serialised [`Session`] that holds no password.
//! - [`Client::fetch_nodes`]: the whole tree, spooled to a temporary file and
//!   streamed ([`Snapshot::scan`]); [`Session::decrypt_node`] decrypts one node.
//! - [`Client::read_range`]: ranged, decrypted reads (AES-128-CTR from any offset);
//!   whole-file reads check the condensed MAC.
//! - [`Client::upload_begin`] / [`Client::upload_send`] / [`Client::upload_complete`]:
//!   chunked uploads that resume from a serialisable [`UploadState`].
//! - [`Client::poll_events`] / [`Client::event_stream`]: the `sc` feed as typed
//!   [`Event`]s with a caller-persisted `sn`.
//! - MEGA's `X-Hashcash` challenge is answered transparently ([`hashcash`]).

pub mod client;
pub mod crypto;
pub mod download;
pub mod error;
pub mod events;
pub mod hashcash;
#[cfg(feature = "mock")]
pub mod mock;
pub mod nodes;
pub mod protocol;
pub mod session;
pub mod upload;

pub use client::{Client, ClientConfig, DEFAULT_API_ORIGIN, ScReply};
pub use crypto::{Fingerprint, NodeAttributes, NodeKey};
pub use download::{ByteStream, DownloadTarget};
pub use error::{Error, ErrorCode, Result};
pub use events::{Event, EventBatch, Packet, Poll};
pub use nodes::{Node, Snapshot};
pub use protocol::{FileNode, NodeKind};
pub use session::Session;
pub use upload::{DEFAULT_POST_MAX, UploadState, file_fingerprint, idempotence_id};

use protocol::{QuotaResponse, Request, UserInfoResponse};

/// The signed-in account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub handle: String,
    pub email: String,
}

/// Storage quota, bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageQuota {
    pub total: u64,
    pub used: u64,
}

impl Client {
    /// `ug`.
    pub async fn user_info(&self) -> Result<UserInfo> {
        let r: UserInfoResponse = serde_json::from_value(self.cs1(Request::UserInfo {}).await?)?;
        Ok(UserInfo {
            handle: r.user,
            email: r.email,
        })
    }

    /// `uq`.
    pub async fn storage_quota(&self) -> Result<StorageQuota> {
        let r: QuotaResponse =
            serde_json::from_value(self.cs1(Request::Quota { xfer: 1, strg: 1 }).await?)?;
        Ok(StorageQuota {
            total: r.total,
            used: r.used,
        })
    }
}
