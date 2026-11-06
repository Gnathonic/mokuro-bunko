//! `<base>/cloud/credentials.json` (§7 Storage, D12).
//!
//! - One entry per store id: tokens, account, the client used, a MEGA session blob
//!   or a WebDAV password.
//! - The directory is 0700 and the file 0600 on unix; on Windows the profile's
//!   default ACL applies. Writes are atomic (temp file, fsync, rename, fsync dir).
//! - Kept apart from `mokuro.db`, so a DB backup carries no tokens.
//! - When `MOKURO_SECRETS_KEY` or `MOKURO_SECRETS_KEY_FILE` is set, the file is
//!   sealed with ChaCha20-Poly1305: `{"sealed": base64(nonce || ciphertext)}`. A
//!   plain file is read once a key is set and comes back sealed on the next write.
//! - Secrets never reach a `Debug` string: every secret field prints `<redacted>`.
//!
//! The MEGA session blob is password-equivalent: whoever holds it is signed in.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const FILE_NAME: &str = "credentials.json";
pub const ENV_KEY: &str = "MOKURO_SECRETS_KEY";
pub const ENV_KEY_FILE: &str = "MOKURO_SECRETS_KEY_FILE";

/// Binds a sealed blob to its purpose.
const AAD: &[u8] = b"mokuro-bunko credentials.json v1";
const NONCE_LEN: usize = 12;
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CredError {
    #[error("credentials file: {0}")]
    Io(#[from] io::Error),
    #[error("credentials.json is sealed: set MOKURO_SECRETS_KEY or MOKURO_SECRETS_KEY_FILE")]
    Sealed,
    #[error(
        "credentials.json could not be unsealed: the MOKURO_SECRETS_KEY does not match, or the file is damaged"
    )]
    WrongKey,
    #[error("credentials.json is damaged: {0}")]
    Corrupt(String),
    #[error("MOKURO_SECRETS_KEY_FILE {path}: {source}")]
    KeyFile { path: PathBuf, source: io::Error },
    #[error(
        "the secrets key must be 32 random bytes in base64 (generate one with `openssl rand -base64 32`); passphrases are not accepted"
    )]
    BadKey,
}

/// The 32-byte sealing key.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey([u8; 32]);

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

impl SecretKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        SecretKey(bytes)
    }

    /// Base64 (standard or URL-safe, padded or not) of exactly 32 bytes. Anything else
    /// is refused: a passphrase hashed once would be cheap to brute-force from a copy
    /// of the sealed file, and a random key needs no slow KDF.
    pub fn from_material(material: &str) -> Result<Self, CredError> {
        let m = material.trim();
        for engine in [&STANDARD, &URL_SAFE] {
            if let Ok(b) = engine.decode(m)
                && let Ok(k) = <[u8; 32]>::try_from(b.as_slice())
            {
                return Ok(SecretKey(k));
            }
        }
        if let Ok(b) = URL_SAFE_NO_PAD.decode(m.trim_end_matches('='))
            && let Ok(k) = <[u8; 32]>::try_from(b.as_slice())
        {
            return Ok(SecretKey(k));
        }
        Err(CredError::BadKey)
    }

    /// The key from `MOKURO_SECRETS_KEY` (preferred) or `MOKURO_SECRETS_KEY_FILE`;
    /// `None` when neither is set (or both are empty).
    pub fn from_env() -> Result<Option<SecretKey>, CredError> {
        let key = std::env::var(ENV_KEY).ok();
        let file = std::env::var_os(ENV_KEY_FILE).map(PathBuf::from);
        Self::from_sources(key.as_deref(), file.as_deref())
    }

    /// [`Self::from_env`] with the values passed in.
    pub fn from_sources(
        key: Option<&str>,
        key_file: Option<&Path>,
    ) -> Result<Option<SecretKey>, CredError> {
        if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
            return Self::from_material(k).map(Some);
        }
        let Some(path) = key_file.filter(|p| !p.as_os_str().is_empty()) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path).map_err(|source| CredError::KeyFile {
            path: path.to_path_buf(),
            source,
        })?;
        if text.trim().is_empty() {
            return Err(CredError::KeyFile {
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "the key file is empty"),
            });
        }
        Self::from_material(&text).map(Some)
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(Key::from_slice(&self.0))
    }
}

/// One store's credentials. Which fields are set depends on the provider:
/// OAuth (gdrive, onedrive): tokens and `client_id`; MEGA: `mega_session`; WebDAV:
/// `webdav_password`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredential {
    /// The `storage.library.backend` name: `gdrive`, `onedrive`, `mega`, `webdav`.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(
        default,
        with = "time::serde::timestamp::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    /// The OAuth client the tokens were issued to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Password-equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mega_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_password: Option<String>,
}

fn redact(v: &Option<String>) -> Option<&'static str> {
    v.as_ref().map(|_| "<redacted>")
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredential")
            .field("provider", &self.provider)
            .field("refresh_token", &redact(&self.refresh_token))
            .field("access_token", &redact(&self.access_token))
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("account_label", &self.account_label)
            .field("client_id", &self.client_id)
            .field("mega_session", &redact(&self.mega_session))
            .field("webdav_password", &redact(&self.webdav_password))
            .finish()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Plain {
    version: u32,
    #[serde(default)]
    stores: BTreeMap<String, StoredCredential>,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    sealed: String,
}

/// The credentials file. Every call reads the file afresh (it is tiny); writes are
/// serialised by an in-process lock.
pub struct CredentialStore {
    dir: PathBuf,
    path: PathBuf,
    key: Option<SecretKey>,
    lock: Mutex<()>,
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &self.path)
            .field("sealed", &self.key.is_some())
            .finish()
    }
}

impl CredentialStore {
    /// `<cloud_dir>/credentials.json`, sealed with the key from the environment.
    /// Fails now, not at first use, when the file is sealed and the key is missing
    /// or wrong.
    pub fn open(cloud_dir: &Path) -> Result<Self, CredError> {
        Self::open_with_key(cloud_dir, SecretKey::from_env()?)
    }

    pub fn open_with_key(cloud_dir: &Path, key: Option<SecretKey>) -> Result<Self, CredError> {
        let s = CredentialStore {
            dir: cloud_dir.to_path_buf(),
            path: cloud_dir.join(FILE_NAME),
            key,
            lock: Mutex::new(()),
        };
        s.load()?;
        Ok(s)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether writes are sealed.
    pub fn is_sealed(&self) -> bool {
        self.key.is_some()
    }

    pub fn get(&self, store_id: &str) -> Result<Option<StoredCredential>, CredError> {
        let _g = self.lock.lock();
        Ok(self.load()?.stores.remove(store_id))
    }

    pub fn store_ids(&self) -> Result<Vec<String>, CredError> {
        let _g = self.lock.lock();
        Ok(self.load()?.stores.into_keys().collect())
    }

    /// Insert or replace a store's credentials; durable when it returns.
    pub fn put(&self, store_id: &str, cred: StoredCredential) -> Result<(), CredError> {
        let _g = self.lock.lock();
        let mut all = self.load()?;
        all.stores.insert(store_id.to_string(), cred);
        self.save(&all)
    }

    /// Change a store's credentials in place; `false` (and no write) when the
    /// store has none.
    pub fn update(
        &self,
        store_id: &str,
        f: impl FnOnce(&mut StoredCredential),
    ) -> Result<bool, CredError> {
        let _g = self.lock.lock();
        let mut all = self.load()?;
        let Some(c) = all.stores.get_mut(store_id) else {
            return Ok(false);
        };
        f(c);
        self.save(&all)?;
        Ok(true)
    }

    /// Delete a store's credentials (Disconnect step 3). `false` when it had none.
    pub fn remove(&self, store_id: &str) -> Result<bool, CredError> {
        let _g = self.lock.lock();
        let mut all = self.load()?;
        if all.stores.remove(store_id).is_none() {
            return Ok(false);
        }
        self.save(&all)?;
        Ok(true)
    }

    fn load(&self) -> Result<Plain, CredError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Plain::default()),
            Err(e) => return Err(e.into()),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| CredError::Corrupt(e.to_string()))?;
        let plain_value = if let Some(sealed) = value.get("sealed") {
            let key = self.key.as_ref().ok_or(CredError::Sealed)?;
            let blob = sealed
                .as_str()
                .and_then(|s| STANDARD.decode(s).ok())
                .ok_or_else(|| CredError::Corrupt("the sealed blob is not base64".into()))?;
            if blob.len() < NONCE_LEN {
                return Err(CredError::WrongKey);
            }
            let (nonce, ct) = blob.split_at(NONCE_LEN);
            let plain_bytes = key
                .cipher()
                .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: AAD })
                .map_err(|_| CredError::WrongKey)?;
            serde_json::from_slice(&plain_bytes).map_err(|e| CredError::Corrupt(e.to_string()))?
        } else {
            value
        };
        serde_json::from_value(plain_value).map_err(|e| CredError::Corrupt(e.to_string()))
    }

    fn save(&self, all: &Plain) -> Result<(), CredError> {
        let plain = serde_json::to_vec_pretty(&Plain {
            version: FORMAT_VERSION,
            stores: all.stores.clone(),
        })
        .map_err(|e| CredError::Corrupt(e.to_string()))?;
        let out = match &self.key {
            None => plain,
            Some(key) => {
                let nonce: [u8; NONCE_LEN] = rand::random();
                let ct = key
                    .cipher()
                    .encrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: &plain,
                            aad: AAD,
                        },
                    )
                    .map_err(|_| CredError::Corrupt("sealing failed".into()))?;
                let mut blob = nonce.to_vec();
                blob.extend_from_slice(&ct);
                serde_json::to_vec(&Sealed {
                    sealed: STANDARD.encode(blob),
                })
                .map_err(|e| CredError::Corrupt(e.to_string()))?
            }
        };
        self.ensure_dir()?;
        write_atomic(&self.dir, &self.path, &out)?;
        Ok(())
    }

    /// Create the directory 0700, or take group and other access off an existing one
    /// (never adding any).
    fn ensure_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            if let Some(parent) = self.dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::DirBuilder::new().mode(0o700).create(&self.dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            let mode = std::fs::metadata(&self.dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(mode & 0o700))?;
            }
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }
}

/// Write `path` through a 0600 temp file in `dir`: fsync, rename, fsync the dir.
fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(
        ".{FILE_NAME}.{}.tmp",
        hex::encode(rand::random::<[u8; 8]>())
    ));
    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_material() {
        let raw = [7u8; 32];
        for enc in [
            STANDARD.encode(raw),
            URL_SAFE.encode(raw),
            URL_SAFE_NO_PAD.encode(raw),
        ] {
            assert_eq!(
                SecretKey::from_material(&enc).unwrap(),
                SecretKey(raw),
                "{enc}"
            );
            assert_eq!(
                SecretKey::from_material(&format!("{enc}\n")).unwrap(),
                SecretKey(raw)
            );
        }
        // Passphrases and base64 of the wrong length are refused.
        assert!(matches!(
            SecretKey::from_material("correct horse battery staple"),
            Err(CredError::BadKey)
        ));
        let short = STANDARD.encode([1u8; 16]);
        assert!(matches!(
            SecretKey::from_material(&short),
            Err(CredError::BadKey)
        ));
        let pass = SecretKey(raw);
        assert_eq!(format!("{pass:?}"), "SecretKey(<redacted>)");
    }

    #[test]
    fn sources() {
        assert_eq!(SecretKey::from_sources(None, None).unwrap(), None);
        assert_eq!(SecretKey::from_sources(Some("  "), None).unwrap(), None);
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("key");
        let file_key = STANDARD.encode([3u8; 32]);
        let env_key = STANDARD.encode([4u8; 32]);
        std::fs::write(&f, format!("{file_key}\n")).unwrap();
        assert_eq!(
            SecretKey::from_sources(None, Some(&f)).unwrap(),
            Some(SecretKey([3u8; 32]))
        );
        // The variable wins over the file.
        assert_eq!(
            SecretKey::from_sources(Some(&env_key), Some(&f)).unwrap(),
            Some(SecretKey([4u8; 32]))
        );
        // A passphrase in either place is refused.
        assert!(matches!(
            SecretKey::from_sources(Some("env passphrase"), None),
            Err(CredError::BadKey)
        ));
        assert!(matches!(
            SecretKey::from_sources(None, Some(&d.path().join("missing"))),
            Err(CredError::KeyFile { .. })
        ));
        std::fs::write(&f, "\n").unwrap();
        assert!(matches!(
            SecretKey::from_sources(None, Some(&f)),
            Err(CredError::KeyFile { .. })
        ));
    }
}
