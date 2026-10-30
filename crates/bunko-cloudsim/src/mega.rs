//! MEGA, mounted under `{base}/mega/` (the gateway, `ClientConfig.api_origin` or
//! megajs's `gateway`). The mock itself stays in `bunko-mega` (feature `mock`): it needs
//! that crate's crypto internals to encrypt nodes for real. Accounts log in by password
//! (created with [`crate::CloudSim::mega_account`] or `POST /_sim/mega/account`) or
//! resume a session blob.

use serde_json::Value;
use sha2::{Digest, Sha256};

pub use bunko_mega::mock::{INBOX, MegaHub, ROOT, RUBBISH};

use crate::{NewNode, NodeError, NodeInfo, Parent, rfc3339};

/// One MEGA account: the `bunko_mega` mock's knob and helper surface.
pub type MegaHandle = bunko_mega::mock::MockMega;

/// The path prefix of the gateway.
pub const PREFIX: &str = "/mega";

fn as_bool(v: &Value, k: &str) -> Result<bool, String> {
    v.as_bool()
        .ok_or_else(|| format!("knob {k} takes true or false"))
}

fn as_u64(v: &Value, k: &str) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("knob {k} takes a non-negative number"))
}

/// Set one knob on a MEGA account (`POST /_sim/knobs`).
pub fn knob(m: &MegaHandle, k: &str, v: &Value) -> Result<(), String> {
    match k {
        "eagain" => m.eagain(as_u64(v, k)? as usize),
        "overquota" => m.overquota(as_bool(v, k)?),
        "bandwidth_509" => m.bandwidth_509(as_bool(v, k)?),
        "garbage_download" => m.garbage_download(as_bool(v, k)?),
        "require_hashcash" => {
            let e = as_u64(v, k)?;
            m.require_hashcash(u8::try_from(e).map_err(|_| "require_hashcash is 0-255")?);
        }
        "drop_upload_at" => m.drop_upload_at(as_u64(v, k)?),
        "lose_cursor" => {
            if as_bool(v, k)? {
                m.lose_cursor();
            }
        }
        "kill_session" => {
            if as_bool(v, k)? {
                m.kill_session();
            }
        }
        "fail_command" => {
            let cmd = v["command"]
                .as_str()
                .ok_or("fail_command takes {command, code}")?;
            let code = v["code"]
                .as_i64()
                .ok_or("fail_command takes {command, code}")?;
            m.fail_command(cmd, code);
        }
        _ => return Err(format!("no MEGA knob {k:?}")),
    }
    Ok(())
}

/// The folder at `path` under the cloud root, creating what is missing.
fn folder_at(m: &MegaHandle, path: &str) -> String {
    let mut parent = ROOT.to_string();
    for dir in path.split('/').filter(|s| !s.is_empty()) {
        parent = match m.find(&parent, dir).into_iter().next() {
            Some(h) => h,
            None => m.mkdir(&parent, dir),
        };
    }
    parent
}

/// `POST /_sim/node`: a new node with real node crypto, beside any of the same name.
/// MEGA keeps one time per node (`ts`, seconds): the modified time, else the created.
pub fn create_node(m: &MegaHandle, node: &NewNode) -> Result<String, NodeError> {
    let parent = match &node.parent {
        Parent::Root => ROOT.to_string(),
        Parent::Path(path) => folder_at(m, path),
        Parent::Id(h) => match m.node(h) {
            None => return Err(NodeError::Bad(format!("parent {h}: no such node"))),
            Some(n) if n.kind == 0 => {
                return Err(NodeError::Bad(format!("parent {h} is not a folder")));
            }
            Some(_) => h.clone(),
        },
    };
    let ts = node.times().map(|(_, modified)| modified.div_euclid(1000));
    Ok(if node.folder {
        m.mkdir_at(&parent, &node.name, ts)
    } else {
        m.put_at(&parent, &node.name, &node.data, ts)
    })
}

/// Every node but the cloud root, inbox and rubbish bin.
pub fn tree(m: &MegaHandle) -> Vec<NodeInfo> {
    let nodes: std::collections::HashMap<String, bunko_mega::mock::MNode> =
        m.nodes().into_iter().collect();
    let trashed = |h: &str| {
        let mut cur = h;
        for _ in 0..64 {
            if cur == RUBBISH {
                return true;
            }
            match nodes.get(cur) {
                Some(n) if !n.parent.is_empty() => cur = &n.parent,
                _ => return false,
            }
        }
        false
    };
    nodes
        .iter()
        .filter(|(_, n)| n.kind <= 1)
        .map(|(h, n)| {
            let folder = n.kind == 1;
            let plain = if folder { None } else { m.content(h) };
            let at = rfc3339(n.ts * 1000);
            NodeInfo {
                id: h.clone(),
                parent_id: n.parent.clone(),
                name: m.name_of(h).map(|(name, _)| name).unwrap_or_default(),
                folder,
                size: if folder { 0 } else { n.size },
                sha256: plain.map(|(data, _)| hex::encode(Sha256::digest(&data))),
                created: at.clone(),
                modified: at,
                trashed: trashed(h),
                explicitly_trashed: n.parent == RUBBISH,
            }
        })
        .collect()
}
