//! Shared application state handed to every handler.

use crate::http::client_ip::TrustedProxies;
use crate::http::limiter::AuthLimiter;
use bunko_core::Config;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

pub struct Inner {
    /// Live configuration: the admin API mutates it and saves it to `config_path`.
    pub config: RwLock<Config>,
    pub config_path: Option<PathBuf>,
    pub proxies: RwLock<TrustedProxies>,
    /// WebDAV/Basic-auth failures (separate from the login page's, as in 0.5.2).
    pub dav_limiter: AuthLimiter,
    pub login_limiter: AuthLimiter,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

impl AppState {
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Self {
        let proxies = TrustedProxies::new(&config.server.trusted_proxies);
        Self {
            inner: Arc::new(Inner {
                config: RwLock::new(config),
                config_path,
                proxies: RwLock::new(proxies),
                dav_limiter: AuthLimiter::default(),
                login_limiter: AuthLimiter::default(),
            }),
        }
    }
}
