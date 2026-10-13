//! Decrypted nodes and the streamed tree fetch.
//!
//! mega-rs's `fetch_own_nodes` parsed the whole `f` answer into memory and built a
//! `Nodes` map with an O(n²) children pass. Here the answer is spooled to an unnamed
//! temporary file and scanned with a streaming JSON visitor, one [`FileNode`] at a
//! time, as often as the caller needs (spec §5.3: decrypt only inside the subtree).

use std::fmt;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::client::Client;
use crate::crypto::{Fingerprint, NodeAttributes, NodeKey};
use crate::error::{Error, ErrorCode, Result};
use crate::protocol::{FileNode, NodeKind, Request};

/// A decrypted node.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub handle: String,
    /// `None` for the three roots.
    pub parent: Option<String>,
    pub kind: NodeKind,
    pub owner: String,
    /// Empty for the three roots.
    pub name: String,
    pub size: u64,
    /// The server's timestamp (unix seconds): creation, for a new version too.
    pub ts: i64,
    pub key: NodeKey,
    pub fingerprint: Option<Fingerprint>,
    /// An attribute `t` some clients write.
    pub modified_at: Option<i64>,
    /// Attribute fields other than `n`, `c`, `t`, kept for renames.
    pub extra: Option<Box<serde_json::Map<String, serde_json::Value>>>,
}

impl Node {
    pub(crate) fn new(
        f: &FileNode,
        parent: Option<String>,
        key: NodeKey,
        attrs: NodeAttributes,
    ) -> Node {
        let fingerprint = attrs.fingerprint();
        Node {
            handle: f.handle.clone(),
            parent,
            kind: f.kind,
            owner: f.user.clone(),
            name: attrs.name,
            size: f.size.unwrap_or(0),
            ts: f.ts,
            key,
            fingerprint,
            modified_at: attrs.modified_at,
            extra: (!attrs.other.is_empty()).then(|| Box::new(attrs.other)),
        }
    }

    /// The attribute block this node carries (for a rename: change `name`, encrypt).
    pub fn attributes(&self) -> NodeAttributes {
        NodeAttributes {
            name: self.name.clone(),
            fingerprint: self.fingerprint.map(|f| f.serialize()),
            modified_at: self.modified_at,
            other: self.extra.as_deref().cloned().unwrap_or_default(),
        }
    }

    /// Apply an `a`/`u` attribute update.
    pub fn set_attributes(&mut self, attrs: NodeAttributes) {
        self.fingerprint = attrs.fingerprint();
        self.modified_at = attrs.modified_at;
        self.name = attrs.name;
        self.extra = (!attrs.other.is_empty()).then(|| Box::new(attrs.other));
    }

    /// The client mtime (fingerprint, else `t`), unix seconds.
    pub fn mtime(&self) -> Option<i64> {
        self.fingerprint.map(|f| f.modified_at).or(self.modified_at)
    }
}

/// A fetched tree, spooled to a temporary file (deleted on drop).
pub struct Snapshot {
    file: std::fs::File,
}

impl Snapshot {
    /// Visit every node in answer order; returns the `sn` of the snapshot.
    /// Blocking: call it on a blocking thread for large trees.
    pub fn scan(&self, visit: &mut dyn FnMut(FileNode)) -> Result<String> {
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(0))?;
        let mut de = serde_json::Deserializer::from_reader(BufReader::with_capacity(1 << 16, f));
        let out = de
            .deserialize_any(TopVisitor { visit })
            .map_err(|e| Error::Protocol(format!("fetch nodes: {e}")))?;
        match out {
            Top::Code(code) => Err(Error::Api(code)),
            Top::Tree(Some(sn)) => Ok(sn),
            Top::Tree(None) => Err(Error::Protocol("fetch nodes without sn".into())),
        }
    }

    /// The spooled answer's size in bytes.
    pub fn len(&self) -> u64 {
        self.file.metadata().map_or(0, |m| m.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Client {
    /// Fetch the whole tree (`f`, `c: 1`) into a temporary file under `dir`.
    pub async fn fetch_nodes(&self, dir: &Path) -> Result<Snapshot> {
        self.session_or_err()?;
        let std_file = tempfile::tempfile_in(dir)?;
        let mut file = tokio::fs::File::from_std(std_file.try_clone()?);
        self.cs1_to_file(Request::FetchNodes { c: 1 }, &mut file)
            .await?;
        drop(file);
        Ok(Snapshot { file: std_file })
    }
}

enum Top {
    Code(ErrorCode),
    Tree(Option<String>),
}

struct TopVisitor<'a> {
    visit: &'a mut dyn FnMut(FileNode),
}

impl<'de> Visitor<'de> for TopVisitor<'_> {
    type Value = Top;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a cs answer array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Top, A::Error> {
        let first = seq
            .next_element_seed(FirstSeed { visit: self.visit })?
            .ok_or_else(|| serde::de::Error::custom("empty answer"))?;
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(first)
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Top, E> {
        Ok(Top::Code(ErrorCode::from_i64(v)))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Top, E> {
        Ok(Top::Code(ErrorCode::from_i64(v as i64)))
    }
}

struct FirstSeed<'a> {
    visit: &'a mut dyn FnMut(FileNode),
}

impl<'de> DeserializeSeed<'de> for FirstSeed<'_> {
    type Value = Top;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Top, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for FirstSeed<'_> {
    type Value = Top;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a tree object or an error code")
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Top, E> {
        Ok(Top::Code(ErrorCode::from_i64(v)))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Top, E> {
        Ok(Top::Code(ErrorCode::from_i64(v as i64)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Top, A::Error> {
        let mut sn = None;
        while let Some(k) = map.next_key::<String>()? {
            match k.as_str() {
                "f" => map.next_value_seed(NodesSeed {
                    visit: &mut *self.visit,
                })?,
                "sn" => sn = Some(map.next_value::<String>()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Top::Tree(sn))
    }
}

struct NodesSeed<'a> {
    visit: &'a mut dyn FnMut(FileNode),
}

impl<'de> DeserializeSeed<'de> for NodesSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for NodesSeed<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array of nodes")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(n) = seq.next_element::<FileNode>()? {
            (self.visit)(n);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn snap(body: &str) -> Snapshot {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        Snapshot { file: f }
    }

    #[test]
    fn scans_nodes_and_sn_in_any_key_order() {
        let s = snap(
            r#"[{"ok":[{"h":"x"}],"f":[{"h":"ROOT0000","t":2,"p":"","a":"","ts":1},
                {"h":"FOLDER01","t":1,"p":"ROOT0000","a":"AA","k":"u:k","ts":2,"u":"u"}],
                "u":[{"u":"x"}],"sn":"SN123","noc":1}]"#,
        );
        let mut seen = Vec::new();
        let sn = s.scan(&mut |n| seen.push(n.handle)).unwrap();
        assert_eq!(sn, "SN123");
        assert_eq!(seen, vec!["ROOT0000", "FOLDER01"]);
        // A second scan sees the same.
        let mut again = 0;
        s.scan(&mut |_| again += 1).unwrap();
        assert_eq!(again, 2);
    }

    #[test]
    fn scan_reports_error_codes() {
        assert!(matches!(
            snap("[-15]").scan(&mut |_| {}),
            Err(Error::Api(ErrorCode::ESID))
        ));
        assert!(matches!(
            snap("-9").scan(&mut |_| {}),
            Err(Error::Api(ErrorCode::ENOENT))
        ));
        assert!(snap(r#"[{"f":[]}]"#).scan(&mut |_| {}).is_err());
        assert!(snap(r#"[{"f":[{"h":1}]"#).scan(&mut |_| {}).is_err());
    }
}
