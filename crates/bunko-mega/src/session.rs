//! Login, the serialised session, and node-key unwrapping.
//!
//! Vendored from mega-rs `Client::login`, `resume_session`, `serialize_session` and the
//! key handling in `fetch_own_nodes`. The blob format is mega-rs's (`[1] || master key
//! ECB-wrapped with the session's sek || sid`, base64): it holds no password, and the
//! wrapped master key is only usable together with the server, which hands back the
//! `sek` on resume. Changes: errors instead of `todo!()`/`unwrap` (a v2 account
//! without salt, a short `csid`), `privk` optional on resume, keys zeroized on drop.

use zeroize::Zeroize;

use crate::client::{Client, ClientConfig};
use crate::crypto::{
    self, FILE_KEY_SIZE, FOLDER_KEY_SIZE, NodeAttributes, NodeKey, RsaPrivateKey, b64, unb64,
};
use crate::error::{Error, ErrorCode, Result};
use crate::nodes::Node;
use crate::protocol::{FileNode, LoginResponse, NodeKind, PreLoginResponse, Request};

/// Length of a session id, bytes.
pub const SID_LEN: usize = 43;

/// A logged-in session. `Debug` prints nothing secret.
#[derive(Clone)]
pub struct Session {
    sid: String,
    master: [u8; 16],
    sek: [u8; 16],
    user: String,
    privk: Option<RsaPrivateKey>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.sid.zeroize();
        self.master.zeroize();
        self.sek.zeroize();
        if let Some(k) = self.privk.as_mut() {
            k.zeroize();
        }
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Encode a session blob from its parts (what [`Session::serialize`] writes; public
/// for converters and tests).
pub fn encode_blob(master: &[u8; 16], sek: &[u8; 16], sid: &[u8]) -> Result<String> {
    let mut wrapped = *master;
    crypto::ecb_encrypt(sek, &mut wrapped)?;
    let mut out = Vec::with_capacity(1 + 16 + sid.len());
    out.push(1u8);
    out.extend_from_slice(&wrapped);
    out.extend_from_slice(sid);
    wrapped.zeroize();
    let s = b64(&out);
    out.zeroize();
    Ok(s)
}

fn decode_blob(blob: &str) -> Result<([u8; 16], String)> {
    let raw = unb64(blob.trim()).map_err(|_| Error::InvalidSession("not base64".into()))?;
    match raw.first() {
        Some(1) => {}
        Some(2) => return Err(Error::InvalidSession("folder-link session".into())),
        _ => return Err(Error::InvalidSession("unknown session kind".into())),
    }
    if raw.len() < 1 + 16 + 1 {
        return Err(Error::InvalidSession("too short".into()));
    }
    let wrapped: [u8; 16] = raw[1..17]
        .try_into()
        .map_err(|_| Error::InvalidSession("too short".into()))?;
    Ok((wrapped, b64(&raw[17..])))
}

fn b64_16(s: Option<&str>, field: &'static str) -> Result<[u8; 16]> {
    let v = unb64(s.ok_or_else(|| Error::Protocol(format!("login without {field}")))?)?;
    v.try_into()
        .map_err(|_| Error::Protocol(format!("{field} is not 16 bytes")))
}

impl Session {
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// The account's user handle.
    pub fn user_handle(&self) -> &str {
        &self.user
    }

    /// The serialised session (password-equivalent together with the server: store
    /// it encrypted).
    pub fn serialize(&self) -> Result<String> {
        let sid = unb64(&self.sid)?;
        encode_blob(&self.master, &self.sek, &sid)
    }

    /// Wrap a node key with the master key, as `p` stores it.
    pub fn wrap_key(&self, key: &NodeKey) -> Result<String> {
        let mut k = key.packed();
        crypto::ecb_encrypt(&self.master, &mut k)?;
        let s = b64(&k);
        k.zeroize();
        Ok(s)
    }

    /// The node key, when this account can unwrap it (its own entry, or an RSA one).
    pub fn unwrap_key(&self, f: &FileNode) -> Option<NodeKey> {
        let is_file = f.kind.is_file();
        let want = if is_file {
            FILE_KEY_SIZE
        } else {
            FOLDER_KEY_SIZE
        };
        f.key.as_deref()?.split('/').find_map(|entry| {
            let (owner, k) = entry.split_once(':')?;
            if owner != self.user {
                // Share keys (`^!keys`) are not supported: inshares are not ours.
                return None;
            }
            if k.len() >= 44 {
                // RSA-wrapped (MEGA does this for some nodes created by others).
                let data = unb64(k).ok()?;
                let (enc, _) = crypto::get_mpi(&data).ok()?;
                let mut dec = self.privk.as_ref()?.decrypt(enc);
                // Leading zero bytes are dropped by the big-integer conversion.
                if dec.len() < want {
                    return None;
                }
                dec.truncate(want);
                return NodeKey::from_unwrapped(is_file, &dec);
            }
            let mut raw = unb64(k).ok()?;
            if raw.len() != want {
                return None;
            }
            crypto::ecb_decrypt(&self.master, &mut raw).ok()?;
            let key = NodeKey::from_unwrapped(is_file, &raw);
            raw.zeroize();
            key
        })
    }

    /// Decrypt a node. `Ok(None)` for a node this account holds no key for; an error
    /// for a key that unwraps but does not decrypt the attributes.
    pub fn decrypt_node(&self, f: &FileNode) -> Result<Option<Node>> {
        let parent = (!f.parent.is_empty()).then(|| f.parent.clone());
        if matches!(f.kind, NodeKind::Root | NodeKind::Inbox | NodeKind::Trash) {
            return Ok(Some(Node {
                handle: f.handle.clone(),
                parent,
                kind: f.kind,
                owner: f.user.clone(),
                name: String::new(),
                size: 0,
                ts: f.ts,
                key: NodeKey::Folder { aes: [0; 16] },
                fingerprint: None,
                modified_at: None,
                extra: None,
            }));
        }
        let Some(key) = self.unwrap_key(f) else {
            return Ok(None);
        };
        let attrs = NodeAttributes::decrypt_b64(key.aes(), &f.attr)?;
        Ok(Some(Node::new(f, parent, key, attrs)))
    }
}

/// The reason a login did not produce a session.
pub type LoginResult<T> = Result<T>;

impl Client {
    /// Log in with email and password (and a 2FA code when the account has one).
    /// The password is used for key derivation only and not kept.
    pub async fn login(
        cfg: ClientConfig,
        email: &str,
        password: &str,
        mfa: Option<&str>,
    ) -> LoginResult<Client> {
        let anon = Client::new(cfg, None)?;
        let email = email.trim().to_lowercase();
        let pre: PreLoginResponse = serde_json::from_value(
            anon.cs1(Request::PreLogin {
                user: email.clone(),
            })
            .await?,
        )?;
        let (mut login_key, user_hash) = match (pre.version, pre.salt.as_deref()) {
            (1, _) => {
                let key = crypto::prepare_key_v1(password.as_bytes());
                (key, b64(&crypto::user_hash_v1(&key, email.as_bytes())))
            }
            (2, Some(salt)) => {
                let mut derived = crypto::prepare_key_v2(password.as_bytes(), &unb64(salt)?);
                let mut key = [0u8; 16];
                key.copy_from_slice(&derived[..16]);
                let hash = b64(&derived[16..]);
                derived.zeroize();
                (key, hash)
            }
            (2, None) => {
                return Err(Error::Protocol("v2 account without a salt".into()));
            }
            (v, _) => return Err(Error::Protocol(format!("unknown login version {v}"))),
        };
        let mut sek: [u8; 16] = rand::random();
        let resp = anon
            .cs1(Request::Login {
                user: Some(email),
                user_hash: Some(user_hash),
                sek: Some(b64(&sek)),
                mfa: mfa.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
            })
            .await;
        let resp: LoginResponse = match resp {
            Ok(v) => serde_json::from_value(v)?,
            Err(e) => {
                login_key.zeroize();
                return Err(e);
            }
        };
        let mut master = {
            let mut k = unb64(
                resp.key
                    .as_deref()
                    .ok_or_else(|| Error::Protocol("login without k".into()))?,
            )?;
            crypto::ecb_decrypt(&login_key, &mut k)?;
            login_key.zeroize();
            let master: [u8; 16] = k
                .as_slice()
                .try_into()
                .map_err(|_| Error::Protocol("master key is not 16 bytes".into()))?;
            k.zeroize();
            master
        };
        if let Some(s) = resp.sek.as_deref() {
            sek = b64_16(Some(s), "sek")?;
        }
        let privk = {
            let mut p = unb64(
                resp.privk
                    .as_deref()
                    .ok_or_else(|| Error::Protocol("login without privk".into()))?,
            )?;
            let trimmed = p.len() - p.len() % 16;
            crypto::ecb_decrypt(&master, &mut p[..trimmed])?;
            let k = RsaPrivateKey::from_mpi_bytes(&p);
            p.zeroize();
            k.map_err(|_| Error::Crypto("wrong password (private key does not decode)".into()))?
        };
        let csid = unb64(resp.csid.as_deref().ok_or_else(|| {
            Error::Protocol("login without csid (tsid accounts are not supported)".into())
        })?)?;
        let (enc, _) = crypto::get_mpi(&csid)?;
        let dec = privk.decrypt(enc);
        if dec.len() < SID_LEN {
            master.zeroize();
            return Err(Error::Crypto("session id does not decrypt".into()));
        }
        let session = Session {
            sid: b64(&dec[..SID_LEN]),
            master,
            sek,
            user: resp.user.clone(),
            privk: Some(privk),
        };
        master.zeroize();
        anon.with_session(session)
    }

    /// Resume a serialised session. One `us` call recovers the sek and validates the
    /// session: `Error::Api(ESID)` when it was killed or expired.
    pub async fn resume(cfg: ClientConfig, blob: &str) -> Result<Client> {
        let (mut wrapped, sid) = decode_blob(blob)?;
        let anon = Client::new(cfg, None)?;
        let fresh: [u8; 16] = rand::random();
        let v = anon
            .cs1_with(
                Request::Login {
                    user: None,
                    user_hash: None,
                    sek: Some(b64(&fresh)),
                    mfa: None,
                },
                &[("sid", sid.as_str())],
            )
            .await?;
        let resp: LoginResponse = serde_json::from_value(v)?;
        let sek = b64_16(resp.sek.as_deref(), "sek")?;
        crypto::ecb_decrypt(&sek, &mut wrapped)?;
        let master = wrapped;
        wrapped.zeroize();
        let privk = resp.privk.as_deref().and_then(|p| {
            let mut p = unb64(p).ok()?;
            let trimmed = p.len() - p.len() % 16;
            crypto::ecb_decrypt(&master, &mut p[..trimmed]).ok()?;
            let k = RsaPrivateKey::from_mpi_bytes(&p).ok();
            p.zeroize();
            k
        });
        if resp.user.is_empty() {
            return Err(Error::Protocol("resume without a user handle".into()));
        }
        anon.with_session(Session {
            sid,
            master,
            sek,
            user: resp.user,
            privk,
        })
    }

    /// End this session server-side (other sessions, the reader's included, stay).
    pub async fn logout(&self) -> Result<()> {
        match self.cs1(Request::Logout {}).await {
            Ok(_) => Ok(()),
            Err(Error::Api(ErrorCode::ESID)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trip_and_rejects() {
        let sid = [7u8; SID_LEN];
        let blob = encode_blob(&[1; 16], &[2; 16], &sid).unwrap();
        let (wrapped, sid_b64) = decode_blob(&blob).unwrap();
        let mut m = wrapped;
        crypto::ecb_decrypt(&[2; 16], &mut m).unwrap();
        assert_eq!(m, [1; 16]);
        assert_eq!(unb64(&sid_b64).unwrap(), sid);
        assert!(decode_blob("AgAA").is_err());
        assert!(decode_blob("!!").is_err());
        assert!(decode_blob(&b64(&[1, 2, 3])).is_err());
    }

    #[test]
    fn debug_hides_secrets() {
        let s = Session {
            sid: "SECRETSID".into(),
            master: [9; 16],
            sek: [8; 16],
            user: "user".into(),
            privk: None,
        };
        let d = format!("{s:?}");
        assert!(!d.contains("SECRETSID"));
        assert!(d.contains("user"));
    }
}
