//! The `cs` command and response shapes this crate uses.
//!
//! Vendored from mega-rs `protocol/commands.rs`, trimmed to the commands Bunko needs,
//! without `secrecy` wrappers (no `Debug` on requests instead, since `Login` carries a
//! password-derived hash), with optional fields defaulted instead of required, and
//! `NodeKind` decoded leniently.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The kind of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    File,
    Folder,
    /// The Cloud Drive root.
    Root,
    /// The Inbox (Vault) root.
    Inbox,
    /// The Rubbish Bin root.
    Trash,
    Unknown(i64),
}

impl NodeKind {
    pub fn from_i64(n: i64) -> NodeKind {
        match n {
            0 => NodeKind::File,
            1 => NodeKind::Folder,
            2 => NodeKind::Root,
            3 => NodeKind::Inbox,
            4 => NodeKind::Trash,
            n => NodeKind::Unknown(n),
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            NodeKind::File => 0,
            NodeKind::Folder => 1,
            NodeKind::Root => 2,
            NodeKind::Inbox => 3,
            NodeKind::Trash => 4,
            NodeKind::Unknown(n) => n,
        }
    }

    pub fn is_file(self) -> bool {
        self == NodeKind::File
    }

    /// Anything that holds children (folders and the three roots).
    pub fn is_container(self) -> bool {
        matches!(
            self,
            NodeKind::Folder | NodeKind::Root | NodeKind::Inbox | NodeKind::Trash
        )
    }
}

impl Serialize for NodeKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(self.as_i64())
    }
}

impl<'de> Deserialize<'de> for NodeKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(NodeKind::from_i64(i64::deserialize(d)?))
    }
}

/// A node as `f` (fetch nodes), `p` (put nodes) and `t` packets carry it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileNode {
    #[serde(rename = "h")]
    pub handle: String,
    /// Empty for the three roots.
    #[serde(rename = "p", default)]
    pub parent: String,
    #[serde(rename = "t")]
    pub kind: NodeKind,
    /// The encrypted attribute block (base64).
    #[serde(rename = "a", default)]
    pub attr: String,
    /// `owner:wrappedkey` pairs separated by `/`.
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(rename = "ts", default)]
    pub ts: i64,
    #[serde(rename = "u", default)]
    pub user: String,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// File attributes (thumbnails), unused.
    #[serde(rename = "fa", default, skip_serializing_if = "Option::is_none")]
    pub file_attr: Option<String>,
}

/// A `cs` command.
#[derive(Clone, Serialize)]
#[serde(tag = "a")]
pub enum Request {
    /// Pre-login: the account version and salt.
    #[serde(rename = "us0")]
    PreLogin { user: String },
    /// Login (with `user` + `uh`) or session resume (neither; `sid` in the query).
    #[serde(rename = "us")]
    Login {
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(rename = "uh", skip_serializing_if = "Option::is_none")]
        user_hash: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sek: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mfa: Option<String>,
    },
    /// Log out (kills only this session).
    #[serde(rename = "sml")]
    Logout {},
    /// Current user.
    #[serde(rename = "ug")]
    UserInfo {},
    /// Storage (and transfer) quota.
    #[serde(rename = "uq")]
    Quota { xfer: i32, strg: i32 },
    /// The whole tree.
    #[serde(rename = "f")]
    FetchNodes { c: i32 },
    /// A download URL for node `n`.
    #[serde(rename = "g")]
    Download { g: i32, ssl: i32, n: String },
    /// An upload URL for `s` bytes.
    #[serde(rename = "u")]
    Upload { s: u64, ssl: i32 },
    /// Create nodes under `t` (an upload completion or a new folder).
    #[serde(rename = "p")]
    PutNodes {
        t: String,
        n: Vec<NewNode>,
        #[serde(skip_serializing_if = "String::is_empty")]
        i: String,
    },
    /// Set a node's attributes (rename).
    #[serde(rename = "a")]
    SetAttributes { n: String, attr: String, i: String },
    /// Move node `n` under `t`.
    #[serde(rename = "m")]
    Move { n: String, t: String, i: String },
    /// Delete node `n` (and its subtree) permanently.
    #[serde(rename = "d")]
    Delete { n: String, i: String },
}

impl Request {
    pub fn to_value(&self) -> serde_json::Value {
        // Every variant is a plain struct of strings and integers.
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// One node of a `p` command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewNode {
    /// The upload completion token, or `xxxxxxxx` for a folder.
    pub h: String,
    pub t: NodeKind,
    /// Encrypted attributes.
    pub a: String,
    /// The node key, wrapped with the master key.
    pub k: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreLoginResponse {
    #[serde(rename = "v")]
    pub version: i32,
    #[serde(rename = "s", default)]
    pub salt: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct LoginResponse {
    #[serde(rename = "csid", default)]
    pub csid: Option<String>,
    #[serde(rename = "k", default)]
    pub key: Option<String>,
    #[serde(rename = "sek", default)]
    pub sek: Option<String>,
    #[serde(rename = "privk", default)]
    pub privk: Option<String>,
    #[serde(rename = "u", default)]
    pub user: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfoResponse {
    #[serde(rename = "u")]
    pub user: String,
    #[serde(rename = "email", default)]
    pub email: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaResponse {
    /// Total storage, bytes.
    #[serde(rename = "mstrg", default)]
    pub total: u64,
    /// Used storage, bytes.
    #[serde(rename = "cstrg", default)]
    pub used: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadResponse {
    /// The URL (absent when over quota).
    #[serde(rename = "g", default)]
    pub url: Option<String>,
    #[serde(rename = "s", default)]
    pub size: u64,
    /// An error for this download (`-17` over quota).
    #[serde(rename = "e", default)]
    pub err: Option<i64>,
    /// Seconds until the transfer quota frees up, when MEGA says.
    #[serde(rename = "tl", default)]
    pub time_left: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    #[serde(rename = "p")]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PutNodesResponse {
    #[serde(rename = "f", default)]
    pub nodes: Vec<FileNode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_serialise_like_mega_rs() {
        assert_eq!(
            Request::Download {
                g: 1,
                ssl: 2,
                n: "AbCdEfGh".into()
            }
            .to_value(),
            json!({"a": "g", "g": 1, "ssl": 2, "n": "AbCdEfGh"})
        );
        assert_eq!(
            Request::Login {
                user: None,
                user_hash: None,
                sek: Some("x".into()),
                mfa: None
            }
            .to_value(),
            json!({"a": "us", "sek": "x"})
        );
        let p = Request::PutNodes {
            t: "PARENT00".into(),
            n: vec![NewNode {
                h: "xxxxxxxx".into(),
                t: NodeKind::Folder,
                a: "A".into(),
                k: "K".into(),
            }],
            i: String::new(),
        };
        assert_eq!(
            p.to_value(),
            json!({"a": "p", "t": "PARENT00", "n": [{"h": "xxxxxxxx", "t": 1, "a": "A", "k": "K"}]})
        );
    }

    #[test]
    fn file_node_is_lenient() {
        let root: FileNode = serde_json::from_value(json!({"h": "ROOT0000", "t": 2})).unwrap();
        assert_eq!(root.kind, NodeKind::Root);
        assert_eq!(root.parent, "");
        let odd: FileNode = serde_json::from_value(json!({"h": "X", "t": 9, "p": "Y"})).unwrap();
        assert_eq!(odd.kind, NodeKind::Unknown(9));
    }
}
