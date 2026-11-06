//! Access tokens for provider backends (§7 Refresh and re-auth).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use time::OffsetDateTime;

use super::credentials::{CredError, CredentialStore, StoredCredential};
use super::oauth::{OAuthClient, refresh};
use crate::backend::BErr;

/// Refresh this long before the access token expires.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Where a backend gets its bearer token.
pub trait TokenSource: Send + Sync {
    /// A token valid for at least a few minutes. [`BErr::AuthRevoked`] means the
    /// grant is gone and the caller sets `NeedsReauth`; [`BErr::Transient`] means
    /// try again later.
    fn access_token(&self) -> impl Future<Output = Result<String, BErr>> + Send;
    /// The last token was refused (a 401): the next call refreshes.
    fn invalidate(&self);
}

/// A fixed token (tests, and providers whose token never changes).
#[derive(Clone)]
pub struct StaticToken(pub String);

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

impl TokenSource for StaticToken {
    async fn access_token(&self) -> Result<String, BErr> {
        Ok(self.0.clone())
    }
    fn invalidate(&self) {}
}

struct Cached {
    access_token: String,
    expires_at: OffsetDateTime,
}

fn fresh(expires_at: OffsetDateTime) -> bool {
    expires_at - REFRESH_MARGIN > OffsetDateTime::now_utc()
}

/// An OAuth store's token: cached in memory, refreshed [`REFRESH_MARGIN`] before
/// expiry under this store's mutex, so concurrent callers share one refresh. The
/// refreshed tokens (a rotated Microsoft refresh token above all) are durable in
/// the credentials file before the new access token is handed out.
pub struct RefreshingToken {
    client: OAuthClient,
    store_id: String,
    creds: Arc<CredentialStore>,
    /// Serialises refreshes of this store.
    mutex: tokio::sync::Mutex<()>,
    cached: parking_lot::Mutex<Option<Cached>>,
    /// Set by `invalidate`: the access token on disk is not to be trusted either.
    stale: AtomicBool,
}

impl fmt::Debug for RefreshingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshingToken")
            .field("client", &self.client)
            .field("store_id", &self.store_id)
            .finish_non_exhaustive()
    }
}

fn cred_err(e: CredError) -> BErr {
    BErr::Fatal(e.to_string())
}

impl RefreshingToken {
    pub fn new(
        client: OAuthClient,
        store_id: impl Into<String>,
        creds: Arc<CredentialStore>,
    ) -> Self {
        RefreshingToken {
            client,
            store_id: store_id.into(),
            creds,
            mutex: tokio::sync::Mutex::new(()),
            cached: parking_lot::Mutex::new(None),
            stale: AtomicBool::new(false),
        }
    }

    fn cached_fresh(&self) -> Option<String> {
        self.cached
            .lock()
            .as_ref()
            .filter(|c| fresh(c.expires_at))
            .map(|c| c.access_token.clone())
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&CredentialStore, &str) -> Result<T, CredError> + Send + 'static,
    ) -> Result<T, BErr> {
        let creds = self.creds.clone();
        let id = self.store_id.clone();
        tokio::task::spawn_blocking(move || f(&creds, &id))
            .await
            .map_err(|e| BErr::Fatal(format!("credentials task: {e}")))?
            .map_err(cred_err)
    }

    async fn refreshed(&self) -> Result<String, BErr> {
        let _g = self.mutex.lock().await;
        // Another caller may have refreshed while this one waited.
        if let Some(t) = self.cached_fresh() {
            return Ok(t);
        }
        let stored: Option<StoredCredential> = self.blocking(|c, id| c.get(id)).await?;
        let Some(stored) = stored else {
            return Err(BErr::AuthRevoked);
        };
        if !self.stale.load(Ordering::SeqCst)
            && let (Some(at), Some(exp)) = (&stored.access_token, stored.expires_at)
            && fresh(exp)
        {
            *self.cached.lock() = Some(Cached {
                access_token: at.clone(),
                expires_at: exp,
            });
            return Ok(at.clone());
        }
        let Some(rt) = stored.refresh_token else {
            return Err(BErr::AuthRevoked);
        };
        let t = refresh(&self.client, &rt).await.map_err(BErr::from)?;
        // Persist before use: a rotated refresh token must survive a crash right
        // after this point, or the store would be signed out.
        let (at, exp, new_rt) = (
            t.access_token.clone(),
            t.expires_at,
            t.refresh_token.clone(),
        );
        let found = self
            .blocking(move |c, id| {
                c.update(id, |s| {
                    s.access_token = Some(at);
                    s.expires_at = Some(exp);
                    if new_rt.is_some() {
                        s.refresh_token = new_rt;
                    }
                })
            })
            .await?;
        if !found {
            // Disconnected while refreshing.
            return Err(BErr::AuthRevoked);
        }
        self.stale.store(false, Ordering::SeqCst);
        *self.cached.lock() = Some(Cached {
            access_token: t.access_token.clone(),
            expires_at: t.expires_at,
        });
        Ok(t.access_token)
    }
}

impl TokenSource for RefreshingToken {
    async fn access_token(&self) -> Result<String, BErr> {
        if let Some(t) = self.cached_fresh() {
            return Ok(t);
        }
        self.refreshed().await
    }

    fn invalidate(&self) {
        self.stale.store(true, Ordering::SeqCst);
        *self.cached.lock() = None;
    }
}
