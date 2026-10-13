//! Where the library lives, as the pages change it (0.8 §7, §11; M6): the setup
//! wizard's Connect and Check, the admin Storage panel, the desktop app's and the CLI's
//! `library connect` / `library disconnect` (through the local control API) all go
//! through one [`StorageControl`].
//!
//! A Connect writes the credentials and `storage.library` at once; the server opens
//! the new store when it restarts (the running one cannot be swapped under the library
//! services). The setup wizard restarts at its finish, the admin panel and the CLI
//! right after the sign-in. A Disconnect writes `released`, removes the credentials and
//! marks the store directory, then the server restarts onto an unconnected library.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bunko_core::Config;
use bunko_core::config::{LibraryBackend, LibraryConfig};
use bunko_remote::auth::CredentialStore;
use bunko_store::{Blocked, Health};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::connect::{
    ConnectRequest, ConnectStatus, Connector, LibraryChoice, RequestOrigin, pairing_label,
    pairing_of_client, provider_name, store_ran,
};
use super::disconnect::{DisconnectError, DisconnectMode, DisconnectReport, remove_store};
use super::{StoreHandle, open_library_store};

/// Reads the configuration the server runs with.
pub type ReadConfig = Arc<dyn Fn() -> Config + Send + Sync>;
/// Changes the configuration and saves it.
pub type WriteConfig = Arc<dyn Fn(&mut dyn FnMut(&mut Config)) -> Result<(), String> + Send + Sync>;
/// Restarts the server (after the answer went out).
pub type RestartFn = Arc<dyn Fn() + Send + Sync>;

/// How long after a Connect or Disconnect answer the server restarts.
pub const RESTART_DELAY: Duration = Duration::from_millis(1500);

/// See the module docs.
pub struct StorageControl {
    cloud_dir: PathBuf,
    handle: Option<Arc<StoreHandle>>,
    connector: Connector,
    read: ReadConfig,
    write: WriteConfig,
    restart: Mutex<Option<RestartFn>>,
    /// A Connect or Disconnect changed what the server should open.
    restart_pending: AtomicBool,
    /// One Disconnect at a time.
    busy: Mutex<()>,
}

impl std::fmt::Debug for StorageControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageControl")
            .field("cloud_dir", &self.cloud_dir)
            .field("mounted", &self.handle.as_ref().map(|h| &h.store_id))
            .finish_non_exhaustive()
    }
}

impl StorageControl {
    /// `handle`: the cloud store the server mounted (`None`: the library is on this
    /// disk). `connector`: `None` builds one on the live providers.
    pub fn new(
        cloud_dir: PathBuf,
        handle: Option<Arc<StoreHandle>>,
        read: ReadConfig,
        write: WriteConfig,
        connector: Option<Connector>,
    ) -> Arc<StorageControl> {
        let connector = connector.unwrap_or_else(|| {
            let (r, w) = (read.clone(), write.clone());
            Connector::new(
                cloud_dir.clone(),
                Arc::new(move || r().storage.library),
                Arc::new(move |choice: &LibraryChoice| {
                    w(&mut |c: &mut Config| choice.apply_to(&mut c.storage.library))
                }),
            )
        });
        let control = Arc::new(StorageControl {
            cloud_dir,
            handle,
            connector,
            read,
            write,
            restart: Mutex::new(None),
            restart_pending: AtomicBool::new(false),
            busy: Mutex::new(()),
        });
        // "Use this account's library instead": the old store's Disconnect, drained.
        let weak = Arc::downgrade(&control);
        control.connector.set_release_old(Arc::new(move |id: &str| {
            let Some(c) = weak.upgrade() else {
                return Err("the server is stopping".into());
            };
            c.disconnect_store(id, &DisconnectMode::Drain(super::disconnect::DEFAULT_DRAIN))
                .map(|_| ())
                .map_err(|e| match e {
                    DisconnectError::Owed(o) => format!(
                        "the other account's library still has {}: disconnect it first in Storage (wait, export or discard)",
                        o.describe()
                    ),
                    DisconnectError::Failed(e) => e,
                })
        }));
        control
    }

    /// The live config of a running server (`core`), saved where it came from.
    pub fn for_core(
        core: &crate::core::Core,
        handle: Option<Arc<StoreHandle>>,
    ) -> Arc<StorageControl> {
        let (rc, wc) = (core.clone(), core.clone());
        Self::new(
            core.layout.cloud_dir(),
            handle,
            Arc::new(move || rc.config.read().clone()),
            Arc::new(move |f: &mut dyn FnMut(&mut Config)| {
                {
                    let mut c = wc.config.write();
                    f(&mut c);
                }
                wc.save_config().map_err(|e| e.to_string())
            }),
            None,
        )
    }

    pub fn set_restart(&self, f: RestartFn) {
        *self.restart.lock() = Some(f);
    }

    pub fn connector(&self) -> &Connector {
        &self.connector
    }

    /// The mounted cloud store (`None`: the library is on this disk).
    pub fn handle(&self) -> Option<&Arc<StoreHandle>> {
        self.handle.as_ref()
    }

    pub fn config(&self) -> Config {
        (self.read)()
    }

    /// A Connect or Disconnect is waiting for a restart to take effect.
    pub fn restart_pending(&self) -> bool {
        if self.restart_pending.load(Ordering::SeqCst) {
            return true;
        }
        let s = self.connector.status();
        s.state == "connected"
            && s.store_id.as_deref() != self.handle.as_ref().and_then(|h| h.store_id.as_deref())
    }

    /// Restart the server soon (after this answer goes out); false without a hook.
    pub fn restart_soon(&self) -> bool {
        let Some(f) = self.restart.lock().clone() else {
            return false;
        };
        tracing::info!("[STORAGE] Restarting the server to apply the library change");
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_DELAY).await;
            f();
        });
        true
    }

    /// `library.storage`: the mounted store's block (or a library on this disk), the
    /// sign-in in progress (`connect`), and what the pages need to offer Connect.
    pub fn status_json(&self) -> Value {
        let config = (self.read)();
        let mut v = match &self.handle {
            Some(h) => h.status_json(),
            None => json!({
                "backend": "local",
                "provider": provider_name(LibraryBackend::Local),
                "folder": config.storage.layout().library(),
                "connected": true,
                "health": {"state": "online"},
                "problems": [],
            }),
        };
        let lib = &config.storage.library;
        v["connect"] = self.connector.status().as_json();
        v["restart_pending"] = json!(self.restart_pending());
        v["configured"] = configured_json(lib);
        v["serve_readers"] = json!(config.server.serve_readers);
        v["progress_user"] = json!(lib.progress_user);
        v
    }

    /// Start a sign-in. `restart_after`: restart the server once it connected (admin,
    /// CLI); the setup wizard restarts at its finish instead.
    pub fn connect(
        self: &Arc<Self>,
        req: ConnectRequest,
        origin: RequestOrigin,
        restart_after: bool,
    ) -> Result<ConnectStatus, String> {
        let status = self.connector.start(req, origin)?;
        if restart_after {
            self.restart_when_connected(status.session);
        }
        Ok(status)
    }

    /// The account choice of `account-mismatch` (§7).
    pub async fn decide_account(
        self: &Arc<Self>,
        use_this_account: bool,
        restart_after: bool,
    ) -> Result<ConnectStatus, String> {
        let s = self.connector.decide_account(use_this_account).await?;
        if restart_after && s.state == "connected" {
            self.restart_pending.store(true, Ordering::SeqCst);
            self.restart_soon();
        }
        Ok(s)
    }

    fn restart_when_connected(self: &Arc<Self>, session: u64) {
        let me = self.clone();
        tokio::spawn(async move {
            let s = me
                .connector
                .wait_for(
                    |s| s.session != session || !(s.in_progress() || s.state == "starting"),
                    Duration::from_secs(3 * 3600),
                )
                .await;
            if s.session == session && s.state == "connected" {
                me.restart_pending.store(true, Ordering::SeqCst);
                me.restart_soon();
            }
        });
    }

    /// The library on this disk again (`backend: local`). Refused while a cloud store
    /// that ran here still has its credentials: that needs its Disconnect.
    pub fn use_local(&self) -> Result<(), String> {
        if let Some(name) = self.connected_store_on_file() {
            return Err(format!(
                "this Bunko manages a {name} library: disconnect it first (it is a different library)"
            ));
        }
        let was_cloud = self.handle.is_some();
        (self.write)(&mut |c: &mut Config| c.storage.library.backend = LibraryBackend::Local)?;
        if was_cloud {
            self.restart_pending.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// The provider name of a store with credentials that ran here.
    fn connected_store_on_file(&self) -> Option<&'static str> {
        let creds = CredentialStore::open(&self.cloud_dir).ok()?;
        for id in creds.store_ids().ok()? {
            if store_ran(&self.cloud_dir, &id)
                && let Ok(Some(c)) = creds.get(&id)
            {
                return Some(
                    LibraryBackend::parse(&c.provider)
                        .map(provider_name)
                        .unwrap_or("cloud"),
                );
            }
        }
        None
    }

    /// The admin's Take over (§9.2).
    pub fn take_over(&self) -> Result<(), String> {
        match &self.handle {
            Some(h) => h.take_over().map_err(|e| e.to_string()),
            None => Err("the library is on this disk".into()),
        }
    }

    /// Disconnect the cloud library (§7). Blocking. The mounted store when one runs;
    /// otherwise the store on file (a pairing change, a server that could not open
    /// it), opened here to drain it. Afterwards a restart is pending.
    pub fn disconnect(&self, mode: &DisconnectMode) -> Result<DisconnectReport, DisconnectError> {
        let _one = self.busy.lock();
        let report = match &self.handle {
            Some(h) if h.connected() => h.disconnect(&self.cloud_dir, mode)?,
            _ => self.disconnect_on_file(mode)?,
        };
        self.connector.cancel();
        self.restart_pending.store(true, Ordering::SeqCst);
        Ok(report)
    }

    /// Disconnect one store by id: the mounted one, or one opened here for it.
    fn disconnect_store(
        &self,
        id: &str,
        mode: &DisconnectMode,
    ) -> Result<DisconnectReport, DisconnectError> {
        let _one = self.busy.lock();
        if let Some(h) = &self.handle
            && h.connected()
            && h.store_id.as_deref() == Some(id)
        {
            return h.disconnect(&self.cloud_dir, mode);
        }
        self.disconnect_one_on_file(id, mode)
    }

    /// Every store with credentials here (those that ran first), each disconnected.
    fn disconnect_on_file(
        &self,
        mode: &DisconnectMode,
    ) -> Result<DisconnectReport, DisconnectError> {
        let creds = CredentialStore::open(&self.cloud_dir)
            .map_err(|e| DisconnectError::Failed(e.to_string()))?;
        let ids = creds
            .store_ids()
            .map_err(|e| DisconnectError::Failed(e.to_string()))?;
        let mut last = DisconnectReport::default();
        let (ran, fresh): (Vec<String>, Vec<String>) = ids
            .into_iter()
            .partition(|id| store_ran(&self.cloud_dir, id));
        for id in fresh {
            last.credentials_removed |= remove_store(&self.cloud_dir, &id)
                .map_err(|e| DisconnectError::Failed(e.to_string()))?;
            last.store_id = Some(id);
        }
        for id in ran {
            last = self.disconnect_one_on_file(&id, mode)?;
        }
        Ok(last)
    }

    /// Open the store `id` on file here (its own provider and pairing), then run its
    /// Disconnect. A store that cannot be opened can still be discarded.
    fn disconnect_one_on_file(
        &self,
        id: &str,
        mode: &DisconnectMode,
    ) -> Result<DisconnectReport, DisconnectError> {
        let creds = CredentialStore::open(&self.cloud_dir)
            .map_err(|e| DisconnectError::Failed(e.to_string()))?;
        let Some(cred) = creds
            .get(id)
            .map_err(|e| DisconnectError::Failed(e.to_string()))?
        else {
            return Ok(DisconnectReport {
                store_id: Some(id.to_string()),
                ..DisconnectReport::default()
            });
        };
        let mut config = (self.read)();
        if let Some(b) = LibraryBackend::parse(&cred.provider) {
            config.storage.library.backend = b;
        }
        if let Some(p) = cred.client_id.as_deref().and_then(pairing_of_client) {
            config.storage.library.google_project = p;
        }
        let layout = config.storage.layout();
        let opened = open_library_store(&config, &layout)
            .map_err(|e| DisconnectError::Failed(e.to_string()))?;
        match opened {
            Some((_store, _guard, handle)) if handle.store_id.as_deref() == Some(id) => {
                handle.disconnect(&self.cloud_dir, mode)
            }
            _ if *mode == DisconnectMode::Discard => {
                let removed = remove_store(&self.cloud_dir, id)
                    .map_err(|e| DisconnectError::Failed(e.to_string()))?;
                Ok(DisconnectReport {
                    store_id: Some(id.to_string()),
                    credentials_removed: removed,
                    ..DisconnectReport::default()
                })
            }
            _ => Err(DisconnectError::Failed(format!(
                "the library on file (store {id}) cannot be opened with this configuration to upload what it still owes: choose Discard to drop it, or restore its settings first"
            ))),
        }
    }
}

/// What the configuration names (it may differ from the mounted store until the
/// restart), and what a Connect may offer.
fn configured_json(lib: &LibraryConfig) -> Value {
    let custom = lib.oauth.google.any() || lib.google_project.as_str() == "custom";
    let (loopback, device) = lib.google_flows();
    json!({
        "backend": lib.backend.as_str(),
        "folder": lib.folder,
        "google_project": lib.google_project.as_str(),
        "custom_pairing": custom,
        "custom_pairing_label": custom.then(|| pairing_label(&format!(
            "custom:{}",
            lib.oauth.google.client_id.as_deref().or(lib.oauth.google.device_client_id.as_deref()).unwrap_or("")
        ))),
        "google_flows": {"loopback": loopback, "device": device},
        "webdav": {
            "url": lib.webdav.url,
            "username": lib.webdav.username,
            "allow_http": lib.webdav.allow_http,
        },
        "pinned": {
            "backend": std::env::var("MOKURO_LIBRARY_BACKEND").ok().filter(|v| !v.trim().is_empty()),
            "google_project": std::env::var("MOKURO_GOOGLE_PROJECT").ok().filter(|v| !v.trim().is_empty()),
            "serve_readers": std::env::var("MOKURO_SERVE_READERS").ok().filter(|v| !v.trim().is_empty()),
        },
    })
}

fn hhmm(t: std::time::SystemTime) -> String {
    let t = time::OffsetDateTime::from(t);
    format!("{:02}:{:02} UTC", t.hour(), t.minute())
}

/// The store's `problems[]` (§11): kinds `storage-auth`, `storage-offline`,
/// `storage-throttled`, `storage-full`, `storage-transfer-quota`, `storage-conflict`,
/// `storage-lease`. Each `{kind, severity, text, hint}`.
pub fn storage_problems(h: &StoreHandle, health: &Health, conflicts: usize) -> Vec<Value> {
    let name = provider_name(h.backend);
    let mut out = Vec::new();
    let mut push = |kind: &str, severity: &str, text: String, hint: Option<String>| {
        out.push(json!({"kind": kind, "severity": severity, "text": text, "hint": hint}));
    };
    if let Some((configured, connected)) = &h.pairing_change {
        push(
            "storage-auth",
            "fail",
            "The Google pairing changed; disconnect the old library first".into(),
            Some(format!(
                "Configured: {}. Connected: {}. Disconnect the old library in Storage (or `mokuro-bunko library disconnect`), then connect with the new pairing.",
                pairing_label(configured),
                pairing_label(connected)
            )),
        );
    } else if !h.connected() {
        push(
            "storage-auth",
            "fail",
            format!("The {name} library is not connected"),
            h.problem().map(str::to_string),
        );
    }
    match health {
        Health::NeedsReauth if h.connected() => {
            let hint = (h.backend == LibraryBackend::GDrive).then(|| {
                "Access was removed. Signing out of Google Drive in Mokuro Reader does this, because it removes access for every Mokuro app.".to_string()
            });
            push(
                "storage-auth",
                "fail",
                format!("Sign in to {name} again"),
                hint,
            );
        }
        Health::Offline => push(
            "storage-offline",
            "warn",
            format!("{name} cannot be reached: Bunko keeps retrying"),
            None,
        ),
        Health::Degraded { retry_at } => push(
            "storage-throttled",
            "warn",
            format!(
                "{name} asked Bunko to slow down: retrying at {}",
                hhmm(*retry_at)
            ),
            None,
        ),
        Health::Blocked(Blocked::StorageFull) => push(
            "storage-full",
            "fail",
            format!("The {name} storage is full: results wait in the outbox"),
            Some("Free some space in the cloud account, or get more.".into()),
        ),
        Health::Blocked(_) => push(
            "storage-transfer-quota",
            "warn",
            format!("Paused: {name} transfer quota"),
            Some("Bunko resumes by itself when the quota resets.".into()),
        ),
        Health::LeaseHeld { by } => push(
            "storage-lease",
            "warn",
            format!("Another Bunko ({by}) manages this library"),
            Some(
                "This Bunko waits as a standby. Take over in Storage if that one is gone for good."
                    .into(),
            ),
        ),
        Health::Suspect => push(
            "storage-conflict",
            "warn",
            "Many files disappeared from the cloud folder at once: Bunko holds those removals"
                .into(),
            Some("Check the folder, then apply the removals in Storage.".into()),
        ),
        _ => {}
    }
    if conflicts > 0 {
        push(
            "storage-conflict",
            "warn",
            format!(
                "{conflicts} conflict{} in the cloud folder",
                if conflicts == 1 { "" } else { "s" }
            ),
            Some("See Storage → Conflicts.".into()),
        );
    }
    out
}
