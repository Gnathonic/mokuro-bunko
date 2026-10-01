//! User-facing account pages and APIs: login + tokens, registration, the account page,
//! the first-run setup wizard and the home page (0.5.2 `login/`, `registration/`,
//! `account/`, `setup/`, `home/` WSGI layers).
//!
//! All of these sit OUTSIDE the WebDAV auth gate in 0.5.2 and authenticate requests
//! themselves; the login endpoints count failures in `core.login_limiter`, never in the
//! WebDAV limiter.
//!
//! Wiring (orchestrator):
//! - `router(deps)` serves every route except `GET /`.
//! - `GET /` is decided per request (setup redirect, catalog redirect, home page for
//!   browsers, otherwise WebDAV). Apply [`root_middleware`] as an outer layer of the
//!   app (it must see requests that end up in the WebDAV fallback), or call
//!   [`root_response`] from the fallback yourself.
//! - Methods 0.5.2 let fall through to WebDAV on these paths (e.g. `PUT /login/x`) get
//!   axum's 405 unless the app sets `Router::method_not_allowed_fallback`; routes whose
//!   0.5.2 answer is a JSON 405 (`/api/health`, `/api/stats`, `/api/register*`) carry
//!   their own fallback and are unaffected.

mod account;
mod home;
mod login;
mod registration;
mod setup;
mod util;

pub use account::format_reading_time;
pub use home::{HealthSource, LibraryCounts, is_browser_request, root_middleware, root_response};
pub use setup::{
    SETUP_TOKEN_COOKIE, SETUP_TOKEN_ENV, SETUP_TOKEN_FILE, SETUP_TOKEN_HEADER, SetupFlag,
    ensure_setup_token, remove_setup_token,
};

use crate::core::Core;
use axum::Router;
use axum::extract::FromRef;
use bunko_db::Database;
use std::sync::Arc;

/// `(username, client_ip)`: a processor's token request was refused (0.5.2
/// `on_processor_login_refused`, fed to the processor registry's failed-login list).
pub type ProcessorLoginRefused = Arc<dyn Fn(&str, &str) + Send + Sync>;
/// `(username, reason)`: cut off every processor connected with this account now.
pub type DropProcessorAccount = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Optional callbacks into services this module does not own.
#[derive(Clone, Default)]
pub struct AccountHooks {
    pub on_processor_login_refused: Option<ProcessorLoginRefused>,
    /// Called after a self-service password change or account deletion. 0.5.2 left
    /// this to the processor API's heartbeat re-check; calling it here cuts a revoked
    /// processor off at once.
    pub drop_processor_account: Option<DropProcessorAccount>,
}

/// What the account/login/registration/setup/home routes need.
#[derive(Clone)]
pub struct AccountsDeps {
    pub core: Core,
    pub db: Arc<Database>,
    pub hooks: AccountHooks,
    /// Library volume counts for `/api/stats` and `/api/health` (the library index).
    /// `None` reports `library_status: "unavailable"` and zero/null counts.
    pub library: Option<Arc<dyn LibraryCounts>>,
    /// The `ocr` block of `/api/health`. `None` reports 0.5.2's no-OCR shape
    /// (`{"backend":"skip","worker_alive":null,"pending":null,"failed":0}`).
    pub health: Option<Arc<dyn HealthSource>>,
    /// First-run state shared by the router and [`root_middleware`]: clone the deps
    /// (not rebuild them) so both see the same flag.
    pub setup: SetupFlag,
}

impl AccountsDeps {
    /// Deps with no hooks or optional sources; reads `MOKURO_SETUP_TOKEN` now.
    pub fn new(core: Core, db: Arc<Database>) -> Self {
        AccountsDeps {
            core,
            db,
            hooks: AccountHooks::default(),
            library: None,
            health: None,
            setup: SetupFlag::from_env(),
        }
    }
}

impl FromRef<AccountsDeps> for Core {
    fn from_ref(d: &AccountsDeps) -> Core {
        d.core.clone()
    }
}

/// Every route of this module except `GET /` (see the module docs).
pub fn router(deps: AccountsDeps) -> Router {
    Router::new()
        .merge(login::routes())
        .merge(account::routes())
        .merge(registration::routes())
        .merge(setup::routes())
        .merge(home::routes())
        .with_state(deps)
}
