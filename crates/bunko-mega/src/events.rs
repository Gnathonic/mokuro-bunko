//! The `sc` action-packet feed, typed.
//!
//! Vendored packet shapes from mega-rs `protocol/events.rs` (`t`, `u`, `d`); new:
//! a list of packets known to be irrelevant to the file tree, [`Event::Resync`] for
//! everything else (mega-rs dropped unknown packets silently), and batch folding with
//! deferred deletes so a move (`d` with `m: 1`, then `t` of the same handle) comes out
//! as one [`Event::Moved`] (megajs storage.mjs does the same). The cursor (`sn`) is
//! the caller's to persist: every [`EventBatch`] carries the one that follows it.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use futures_util::Stream;
use serde::Deserialize;
use serde_json::Value;

use crate::client::{Client, ScReply};
use crate::error::Result;
use crate::protocol::FileNode;

/// Packets that never change the file tree: contacts, user attributes, chats,
/// payments, sets, public links, file attributes (thumbnails), shares and their keys
/// (Bunko only reads nodes the account owns).
pub const IGNORED_PACKETS: &[&str] = &[
    "c", "k", "fa", "ua", "uac", "uec", "psts", "pses", "ph", "se", "ipc", "opc", "upci", "upco",
    "ub", "la", "s", "s2", "mcc", "mcna", "mcpc", "mcfc", "mcfpc", "mcpu", "mcsmp", "mcsmr", "aep",
    "aer", "asp", "asr", "ass", "pk", "sqac",
];

/// One parsed action packet.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// `t`: nodes created (or re-attached by a move).
    Added(Vec<FileNode>),
    /// `u`: new attributes (encrypted with the node key) and timestamp.
    Updated {
        handle: String,
        attr: String,
        ts: Option<i64>,
    },
    /// `d`: a node and its subtree are gone; `moved` when this is half of a move.
    Removed { handle: String, moved: bool },
    /// A packet in [`IGNORED_PACKETS`].
    Ignored(String),
    /// Anything else, or a node packet that does not parse.
    Unknown(String),
}

#[derive(Deserialize)]
struct TPacket {
    t: TNodes,
}

#[derive(Deserialize)]
struct TNodes {
    #[serde(default)]
    f: Vec<FileNode>,
}

#[derive(Deserialize)]
struct UPacket {
    n: String,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    ts: Option<i64>,
}

#[derive(Deserialize)]
struct DPacket {
    n: String,
    #[serde(default)]
    m: Option<i64>,
}

pub fn parse_packet(v: &Value) -> Packet {
    let Some(a) = v.get("a").and_then(Value::as_str) else {
        return Packet::Unknown(v.to_string().chars().take(80).collect());
    };
    let unknown = || Packet::Unknown(a.to_string());
    match a {
        "t" => serde_json::from_value::<TPacket>(v.clone())
            .map_or_else(|_| unknown(), |p| Packet::Added(p.t.f)),
        "u" => match serde_json::from_value::<UPacket>(v.clone()) {
            Ok(UPacket {
                n,
                at: Some(at),
                ts,
            }) => Packet::Updated {
                handle: n,
                attr: at,
                ts,
            },
            // A `u` without attributes (a key or owner change only): nothing to model.
            Ok(UPacket { at: None, .. }) => Packet::Ignored(a.to_string()),
            Err(_) => unknown(),
        },
        "d" => serde_json::from_value::<DPacket>(v.clone()).map_or_else(
            |_| unknown(),
            |p| Packet::Removed {
                handle: p.n,
                moved: p.m == Some(1),
            },
        ),
        a if IGNORED_PACKETS.contains(&a) => Packet::Ignored(a.to_string()),
        _ => unknown(),
    }
}

/// One tree change, after folding a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Added(FileNode),
    /// A known node under a new parent (the `t` half of a move).
    Moved(FileNode),
    Updated {
        handle: String,
        attr: String,
        ts: Option<i64>,
    },
    Removed {
        handle: String,
        /// The `d` said "move" but no `t` followed in this batch.
        moved: bool,
    },
    /// The feed held something this crate does not model: re-list the tree.
    Resync {
        packet: String,
    },
}

/// The events of one `sc` reply and the cursor after them.
#[derive(Debug, Clone, PartialEq)]
pub struct EventBatch {
    pub events: Vec<Event>,
    pub sn: String,
}

/// Fold a batch: deletes wait for the end of the batch, and a delete whose handle a
/// later `t` re-adds becomes a [`Event::Moved`].
pub fn fold_batch(packets: &[Value]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut pending: Vec<(String, bool)> = Vec::new();
    let mut pending_at: HashMap<String, usize> = HashMap::new();
    for p in packets {
        match parse_packet(p) {
            Packet::Added(nodes) => {
                for n in nodes {
                    if let Some(i) = pending_at.remove(&n.handle) {
                        pending[i].0.clear();
                        out.push(Event::Moved(n));
                    } else {
                        out.push(Event::Added(n));
                    }
                }
            }
            Packet::Updated { handle, attr, ts } => out.push(Event::Updated { handle, attr, ts }),
            Packet::Removed { handle, moved } => {
                if !pending_at.contains_key(&handle) {
                    pending_at.insert(handle.clone(), pending.len());
                    pending.push((handle, moved));
                }
            }
            Packet::Ignored(_) => {}
            Packet::Unknown(packet) => out.push(Event::Resync { packet }),
        }
    }
    out.extend(
        pending
            .into_iter()
            .filter(|(h, _)| !h.is_empty())
            .map(|(handle, moved)| Event::Removed { handle, moved }),
    );
    out
}

/// What [`Client::poll_events`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Poll {
    Batch(EventBatch),
    /// Nothing after this `sn`; [`Client::sc_wait`] on the URL blocks until there is.
    Idle {
        wait_url: String,
    },
}

impl Client {
    /// The events after `sn`, without waiting.
    pub async fn poll_events(&self, sn: &str) -> Result<Poll> {
        Ok(match self.sc(sn).await? {
            ScReply::Packets { packets, sn } => Poll::Batch(EventBatch {
                events: fold_batch(&packets),
                sn,
            }),
            ScReply::Wait { url } => Poll::Idle { wait_url: url },
        })
    }

    /// A long-polling stream of batches starting after `sn`; each item's `sn` is the
    /// cursor to persist. Ends after the first error (resync from a fresh tree then).
    pub fn event_stream(
        &self,
        sn: String,
        max_wait: Duration,
    ) -> Pin<Box<dyn Stream<Item = Result<EventBatch>> + Send>> {
        let client = self.clone();
        Box::pin(futures_util::stream::unfold(Some(sn), move |state| {
            let client = client.clone();
            async move {
                let mut sn = state?;
                loop {
                    match client.poll_events(&sn).await {
                        Ok(Poll::Batch(b)) => {
                            sn = b.sn.clone();
                            if b.events.is_empty() {
                                continue;
                            }
                            return Some((Ok(b), Some(sn)));
                        }
                        Ok(Poll::Idle { wait_url }) => {
                            if let Err(e) = client.sc_wait(&wait_url, max_wait).await {
                                return Some((Err(e), None));
                            }
                        }
                        Err(e) => return Some((Err(e), None)),
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(h: &str, p: &str) -> Value {
        json!({"h": h, "p": p, "t": 0, "a": "A", "k": "u:K", "ts": 5, "u": "u", "s": 3})
    }

    #[test]
    fn parses_node_packets() {
        let t = json!({"a": "t", "t": {"f": [node("H1", "P1")]}, "ou": "u"});
        let Packet::Added(n) = parse_packet(&t) else {
            panic!()
        };
        assert_eq!(n[0].handle, "H1");
        assert_eq!(
            parse_packet(&json!({"a": "u", "n": "H1", "u": "x", "at": "AT", "ts": 9})),
            Packet::Updated {
                handle: "H1".into(),
                attr: "AT".into(),
                ts: Some(9)
            }
        );
        assert_eq!(
            parse_packet(&json!({"a": "d", "n": "H1", "m": 1})),
            Packet::Removed {
                handle: "H1".into(),
                moved: true
            }
        );
        assert_eq!(
            parse_packet(&json!({"a": "d", "n": "H1"})),
            Packet::Removed {
                handle: "H1".into(),
                moved: false
            }
        );
        assert_eq!(
            parse_packet(&json!({"a": "ua", "st": "x"})),
            Packet::Ignored("ua".into())
        );
        assert_eq!(
            parse_packet(&json!({"a": "zz"})),
            Packet::Unknown("zz".into())
        );
        assert_eq!(
            parse_packet(&json!({"a": "t", "t": 5})),
            Packet::Unknown("t".into())
        );
        assert!(matches!(parse_packet(&json!(7)), Packet::Unknown(_)));
    }

    #[test]
    fn folds_moves_and_defers_deletes() {
        let packets = vec![
            json!({"a": "d", "n": "MOVED", "m": 1}),
            json!({"a": "d", "n": "GONE"}),
            json!({"a": "t", "t": {"f": [node("MOVED", "NEWPARNT"), node("NEW1", "P")]}}),
            json!({"a": "u", "n": "NEW1", "at": "X"}),
            json!({"a": "fa", "n": "NEW1", "fa": "0*abc"}),
            json!({"a": "d", "n": "GONE"}),
        ];
        let ev = fold_batch(&packets);
        assert_eq!(ev.len(), 4, "{ev:?}");
        assert!(matches!(&ev[0], Event::Moved(n) if n.handle == "MOVED" && n.parent == "NEWPARNT"));
        assert!(matches!(&ev[1], Event::Added(n) if n.handle == "NEW1"));
        assert!(matches!(&ev[2], Event::Updated { handle, .. } if handle == "NEW1"));
        assert_eq!(
            ev[3],
            Event::Removed {
                handle: "GONE".into(),
                moved: false
            }
        );
        // A move whose `t` is in a later batch stays a removal, flagged.
        let ev = fold_batch(&[json!({"a": "d", "n": "X", "m": 1})]);
        assert_eq!(
            ev,
            vec![Event::Removed {
                handle: "X".into(),
                moved: true
            }]
        );
        // Unknown packets ask for a resync, in place.
        let ev = fold_batch(&[
            json!({"a": "newthing"}),
            json!({"a": "t", "t": {"f": [node("A", "B")]}}),
        ]);
        assert_eq!(
            ev[0],
            Event::Resync {
                packet: "newthing".into()
            }
        );
    }
}
