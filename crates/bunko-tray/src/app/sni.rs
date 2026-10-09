//! Linux: a KDE/freedesktop StatusNotifierItem with a com.canonical.dbusmenu menu, over
//! the session D-Bus (ksni on zbus: no GTK, no AppIndicator library). KDE Plasma, Xfce,
//! LXQt, Cinnamon, MATE, Budgie and others show it; GNOME needs the "AppIndicator and
//! KStatusNotifierItem Support" extension (Ubuntu ships it on).
//!
//! ksni rebuilds the menu from [`Sni::menu`] after every [`ksni::Handle::update`] and
//! sends the desktop only what changed. A click runs the item's callback inside ksni's
//! task, which just forwards the item id to the tray's loop.

use super::{App, Sender, Setup, Ui, UserEvent, id};
use crate::icons;
use crate::model::{IconState, MenuModel};
use anyhow::Context;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{MenuItem, TrayMethods};
use std::sync::Arc;
use std::sync::mpsc;

/// What the D-Bus side shows; ksni owns it (behind its lock) and reads it on demand.
struct Sni {
    model: MenuModel,
    autostart: bool,
    send: Sender,
    /// ARGB32 (network byte order) pixmaps of the four icon states.
    pixmaps: Vec<(IconState, ksni::Icon)>,
}

/// A label as dbusmenu takes it: `_` marks a mnemonic, `__` is a literal underscore.
pub fn label(text: &str) -> String {
    text.replace('_', "__")
}

/// RGBA (as the PNGs decode) to the ARGB32, network byte order, that SNI wants.
pub fn argb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect()
}

fn pixmap(state: IconState) -> Option<ksni::Icon> {
    match icons::rgba(state) {
        Ok((rgba, w, h)) => Some(ksni::Icon {
            width: w as i32,
            height: h as i32,
            data: argb(&rgba),
        }),
        Err(e) => {
            tracing::error!("tray icon {}: {e}", state.name());
            None
        }
    }
}

impl Sni {
    fn pixmap(&self, state: IconState) -> Vec<ksni::Icon> {
        self.pixmaps
            .iter()
            .filter(|(s, _)| *s == state)
            .map(|(_, i)| i.clone())
            .collect()
    }

    fn item(&self, id: &'static str, text: &str, enabled: bool) -> MenuItem<Sni> {
        StandardItem {
            label: label(text),
            enabled,
            activate: Box::new(move |me: &mut Sni| (me.send)(UserEvent::Menu(id.to_string()))),
            ..Default::default()
        }
        .into()
    }
}

/// A line of text in the menu: shown, not clickable.
fn line(text: &str) -> MenuItem<Sni> {
    StandardItem {
        label: label(text),
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for Sni {
    // Most hosts open the menu on a right click and send Activate on a left click; the
    // menu is all there is, so a left click opens it too.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "mokuro-bunko".into()
    }

    fn title(&self) -> String {
        "Mokuro Bunko".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        if self.model.icon == IconState::Attention {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_name(&self) -> String {
        // Hosts prefer a theme icon over the pixmap when both are set: leave it empty
        // so the state icon (idle, working, paused, attention) is what shows.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmap(self.model.icon)
    }

    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmap(IconState::Attention)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let mut lines = self.model.tooltip.lines();
        let title = lines.next().unwrap_or("Mokuro Bunko").to_string();
        ksni::ToolTip {
            title,
            description: lines.collect::<Vec<_>>().join("\n"),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let m = &self.model;
        let mut items: Vec<MenuItem<Self>> = m.status_lines.iter().map(|l| line(l)).collect();
        items.push(
            SubMenu {
                label: "Statistics".into(),
                submenu: m.stats_lines.iter().map(|l| line(l)).collect(),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(self.item(
            id::PAUSE_AFTER,
            "Pause after this volume",
            m.can_pause_after,
        ));
        items.push(self.item(id::PAUSE_NOW, "Pause now", m.can_pause_now));
        items.push(self.item(id::PAUSE_HOUR, "Pause for 1 hour", m.can_pause_now));
        items.push(self.item(
            id::PAUSE_TOMORROW,
            "Pause until tomorrow 08:00",
            m.can_pause_now,
        ));
        items.push(self.item(id::RESUME, "Resume", m.can_resume));
        items.push(MenuItem::Separator);
        items.push(self.item(id::DASHBOARD, "Open dashboard", m.can_open_dashboard));
        items.push(self.item(id::LIBRARY, "Open library", m.library_url.is_some()));
        items.push(self.item(id::SETTINGS, "Settings…", true));
        items.push(self.item(id::WIZARD, "Setup wizard…", true));
        items.push(self.item(id::LOGS, "Show logs", true));
        items.push(self.item(id::UPDATES, &m.update_text, true));
        items.push(MenuItem::Separator);
        items.push(
            CheckmarkItem {
                label: "Start at login".into(),
                checked: self.autostart,
                activate: Box::new(|me: &mut Sni| {
                    (me.send)(UserEvent::Menu(id::AUTOSTART.to_string()))
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(self.item(id::QUIT, &m.quit_text, true));
        items
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        // The panel restarted, or the desktop has no tray host (yet): keep running and
        // come back when a StatusNotifierWatcher appears.
        tracing::warn!("the desktop's tray host went away ({reason:?}); waiting for it");
        true
    }

    fn watcher_online(&self) {
        tracing::info!("the desktop's tray host is back");
    }
}

struct SniUi {
    rt: tokio::runtime::Runtime,
    send: Sender,
    handle: Option<ksni::Handle<Sni>>,
}

impl Ui for SniUi {
    fn show(&mut self, m: &MenuModel, autostart: bool) -> anyhow::Result<()> {
        match &self.handle {
            Some(h) => {
                let (m, autostart) = (m.clone(), autostart);
                self.rt.block_on(h.update(move |t: &mut Sni| {
                    t.model = m;
                    t.autostart = autostart;
                }));
            }
            None => {
                let tray = Sni {
                    model: m.clone(),
                    autostart,
                    send: self.send.clone(),
                    pixmaps: [
                        IconState::Idle,
                        IconState::Working,
                        IconState::Paused,
                        IconState::Attention,
                    ]
                    .into_iter()
                    .filter_map(|s| pixmap(s).map(|p| (s, p)))
                    .collect(),
                };
                // Started at login before the panel is up, the watcher may not be there
                // yet: wait for it rather than give up (watcher_offline above).
                let h = self
                    .rt
                    .block_on(tray.assume_sni_available(true).spawn())
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .context(
                        "could not put the icon in the panel (the tray needs a session D-Bus; \
                         `mokuro-bunko doctor` checks it)",
                    )?;
                self.handle = Some(h);
            }
        }
        Ok(())
    }

    fn hide(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
    }
}

pub fn run(setup: Setup) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("tray-dbus")
        .enable_all()
        .build()
        .context("starting the tray's D-Bus runtime")?;
    let (tx, rx) = mpsc::channel::<UserEvent>();
    let send: Sender = {
        let tx = std::sync::Mutex::new(tx);
        Arc::new(move |e| {
            let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(e);
        })
    };
    // SIGTERM/SIGINT (a session ending, `kill`, the update handing over to a new tray):
    // quit as the menu's Quit does, stopping what this tray started.
    {
        let send = send.clone();
        rt.spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            tracing::info!("signal: quitting");
            send(UserEvent::Menu(id::QUIT.to_string()));
        });
    }
    let (mut app, _lock) = App::start(setup, send.clone());
    let mut ui = SniUi {
        rt,
        send,
        handle: None,
    };
    app.refresh_or_fail(&mut ui)?;
    while let Ok(event) = rx.recv() {
        if app.on_event(&mut ui, event) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_keep_underscores() {
        assert_eq!(label("Dr_Stone 01 · 3/196"), "Dr__Stone 01 · 3/196");
    }

    #[test]
    fn pixmaps_are_argb() {
        assert_eq!(argb(&[1, 2, 3, 4, 5, 6, 7, 8]), [4, 1, 2, 3, 8, 5, 6, 7]);
        for s in [
            IconState::Idle,
            IconState::Working,
            IconState::Paused,
            IconState::Attention,
        ] {
            let p = pixmap(s).unwrap();
            assert_eq!(p.data.len() as i32, p.width * p.height * 4);
        }
    }
}
