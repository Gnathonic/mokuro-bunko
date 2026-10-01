//! `Core`: what every HTTP module shares — live config, the auth backend, limiters and
//! client-IP rules. Modules take a `Core` plus their own handles and return a `Router`.

use crate::auth::{self, AnonymousAccess, AuthBackend, Identity};
use crate::http::client_ip::TrustedProxies;
use crate::http::limiter::AuthLimiter;
use axum::extract::{ConnectInfo, FromRequestParts};
use bunko_core::{Config, StorageLayout};
use http::request::Parts;
use parking_lot::RwLock;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct Core(Arc<CoreInner>);

pub struct CoreInner {
    pub config: Arc<RwLock<Config>>,
    pub config_path: Option<PathBuf>,
    pub layout: StorageLayout,
    pub proxies: RwLock<TrustedProxies>,
    pub backend: Arc<dyn AuthBackend>,
    /// WebDAV / Basic failures (separate from the login page's, as in 0.5.2).
    pub dav_limiter: AuthLimiter,
    pub login_limiter: AuthLimiter,
    pub started_at: std::time::Instant,
}

impl std::ops::Deref for Core {
    type Target = CoreInner;
    fn deref(&self) -> &CoreInner {
        &self.0
    }
}

impl Core {
    pub fn new(config: Arc<RwLock<Config>>, config_path: Option<PathBuf>, backend: Arc<dyn AuthBackend>) -> Self {
        let (layout, proxies) = {
            let c = config.read();
            (c.storage.layout(), TrustedProxies::new(&c.server.trusted_proxies))
        };
        Core(Arc::new(CoreInner {
            config,
            config_path,
            layout,
            proxies: RwLock::new(proxies),
            backend,
            dav_limiter: AuthLimiter::default(),
            login_limiter: AuthLimiter::default(),
            started_at: std::time::Instant::now(),
        }))
    }

    /// Re-read `server.trusted_proxies` after a config change.
    pub fn refresh_proxies(&self) {
        let list = self.config.read().server.trusted_proxies.clone();
        *self.proxies.write() = TrustedProxies::new(&list);
    }

    pub fn anonymous_access(&self) -> AnonymousAccess {
        let c = self.config.read();
        AnonymousAccess { browse: c.registration.allow_anonymous_browse, download: c.registration.allow_anonymous_download }
    }

    /// Persist the live config (admin edits).
    pub fn save_config(&self) -> Result<(), bunko_core::ConfigError> {
        let snapshot = self.config.read().clone();
        bunko_core::config::save_config(&snapshot, self.config_path.as_deref())
    }
}

/// Per-request facts: peer, resolved client IP, and the authenticated identity using
/// the WebDAV limiter. Login endpoints that need their own limiter call
/// [`auth::authenticate`] with `core.login_limiter` instead.
#[derive(Debug, Clone)]
pub struct RequestCtx {
    pub peer: IpAddr,
    pub client_ip: String,
    pub identity: Identity,
}

impl RequestCtx {
    pub fn peer_of(parts: &Parts) -> IpAddr {
        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    pub fn resolve(core: &Core, parts: &Parts) -> Self {
        let peer = Self::peer_of(parts);
        let client_ip = core.proxies.read().client_ip_text(peer, &parts.headers);
        let identity = auth::authenticate(&parts.headers, &client_ip, core.backend.as_ref(), &core.dav_limiter);
        RequestCtx { peer, client_ip, identity }
    }
}

impl<S> FromRequestParts<S> for RequestCtx
where
    S: Send + Sync,
    Core: axum::extract::FromRef<S>,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let core = <Core as axum::extract::FromRef<S>>::from_ref(state);
        if let Some(ctx) = parts.extensions.get::<RequestCtx>() {
            return Ok(ctx.clone());
        }
        // Password checks are blocking (bcrypt): keep them off the async workers.
        let ctx = {
            let p = parts.clone();
            tokio::task::spawn_blocking(move || RequestCtx::resolve(&core, &p))
                .await
                .unwrap_or_else(|_| RequestCtx { peer: IpAddr::V4(Ipv4Addr::LOCALHOST), client_ip: String::new(), identity: Identity::default() })
        };
        parts.extensions.insert(ctx.clone());
        Ok(ctx)
    }
}
