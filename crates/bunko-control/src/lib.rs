//! The local control API of a running mokuro-bunko instance (docs/rust-port/GUI.md §2,
//! §3): how the tray and the desktop app pages see and steer `serve`,
//! `processor serve` and `gui` on this machine.
//!
//! * [`types`] — the JSON (status, pause body, `.control.json`); no features needed
//!   (what the tray links).
//! * [`pause`] — the pause state machine (`after_volume` / `now`, `until`,
//!   `.pause.json`). The processor runtime subscribes and does the pausing.
//! * [`activity`] — current volumes, rates, today/total counts (`.stats.json`), fed
//!   with the processor's own ops and events.
//! * [`control`] — [`Control`], the per-instance state the status is built from.
//! * [`http`] — the listener: `127.0.0.1:0`, bearer token / cookie, SSE.
//!
//! # Integration
//!
//! ```text
//! let control = Control::new(ControlConfig::new(Role::Processor, name, VERSION, &storage));
//! control.set_stop(shutdown_token.clone());        // POST /control/stop (managed only)
//! let listener = ControlListener::bind().await?;   // 127.0.0.1:<ephemeral>, new token
//! let app = gui::app(role, listener.token(), listener.login_codes(), ...); // optional: /app pages (G2)
//! let server = listener.serve(control.clone(), Some(app))?;   // writes .control.json
//! ... run, keeping control current (set_link / set_library / set_problems) ...
//! server.shutdown().await;                         // removes .control.json
//! ```

pub mod types;

#[cfg(feature = "runtime")]
pub mod activity;
#[cfg(feature = "runtime")]
pub mod control;
#[cfg(feature = "runtime")]
pub mod file;
#[cfg(feature = "runtime")]
pub mod http;
#[cfg(feature = "runtime")]
pub mod pause;

pub use types::*;

#[cfg(feature = "runtime")]
pub use activity::{Activity, Changes};
#[cfg(feature = "runtime")]
pub use control::{
    Control, ControlConfig, ControlError, LinkPhase, LoadProbe, enabled_from_env, managed_from_env,
};
#[cfg(feature = "runtime")]
pub use http::{ControlListener, ControlServer};
#[cfg(feature = "runtime")]
pub use pause::{PauseCtl, PauseError, PauseState};
