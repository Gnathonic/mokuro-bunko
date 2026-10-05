//! `mokuro-bunko-tray`: the desktop tray for mokuro-bunko (GUI.md §5).
//!
//! The logic lives in plain modules (discovery, the control API client, the menu
//! model, supervision, autostart) so it is testable without a desktop; `app` glues it
//! to the native tray (tray-icon + muda menus on a tao event loop).

pub mod autostart;
pub mod client;
pub mod discover;
pub mod icons;
pub mod model;
pub mod monitor;
pub mod paths;
pub mod status;
pub mod supervise;
pub mod trayconf;
pub mod updates;
