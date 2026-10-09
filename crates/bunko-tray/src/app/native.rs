//! Windows and macOS: the tray icon and its menu are tray-icon + muda, driven by a tao
//! event loop on the main thread (macOS requires it).

use super::{App, Setup, Ui, UserEvent, id};
use crate::icons;
use crate::model::{IconState, MenuModel};
use anyhow::Context;
use std::sync::Arc;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// The menu items whose text or state changes.
struct Items {
    menu: Menu,
    status: Vec<MenuItem>,
    stats: Vec<MenuItem>,
    pause_after: MenuItem,
    pause_now: MenuItem,
    pause_hour: MenuItem,
    pause_tomorrow: MenuItem,
    resume: MenuItem,
    dashboard: MenuItem,
    library: MenuItem,
    updates: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
}

fn build_menu(m: &MenuModel, autostart_on: bool) -> anyhow::Result<Items> {
    let menu = Menu::new();
    let status: Vec<MenuItem> = m
        .status_lines
        .iter()
        .map(|l| MenuItem::new(l, false, None))
        .collect();
    for s in &status {
        menu.append(s)?;
    }
    let stats_menu = Submenu::new("Statistics", true);
    let stats: Vec<MenuItem> = m
        .stats_lines
        .iter()
        .map(|l| MenuItem::new(l, false, None))
        .collect();
    for s in &stats {
        stats_menu.append(s)?;
    }
    menu.append(&stats_menu)?;
    menu.append(&PredefinedMenuItem::separator())?;
    let item = |id: &str, text: &str, enabled: bool| MenuItem::with_id(id, text, enabled, None);
    let pause_after = item(
        id::PAUSE_AFTER,
        "Pause after this volume",
        m.can_pause_after,
    );
    let pause_now = item(id::PAUSE_NOW, "Pause now", m.can_pause_now);
    let pause_hour = item(id::PAUSE_HOUR, "Pause for 1 hour", m.can_pause_now);
    let pause_tomorrow = item(
        id::PAUSE_TOMORROW,
        "Pause until tomorrow 08:00",
        m.can_pause_now,
    );
    let resume = item(id::RESUME, "Resume", m.can_resume);
    menu.append_items(&[
        &pause_after,
        &pause_now,
        &pause_hour,
        &pause_tomorrow,
        &resume,
    ])?;
    menu.append(&PredefinedMenuItem::separator())?;
    let dashboard = item(id::DASHBOARD, "Open dashboard", m.can_open_dashboard);
    let library = item(id::LIBRARY, "Open library", m.library_url.is_some());
    let settings = item(id::SETTINGS, "Settings…", true);
    let wizard = item(id::WIZARD, "Setup wizard…", true);
    let logs = item(id::LOGS, "Show logs", true);
    let updates = item(id::UPDATES, &m.update_text, true);
    menu.append_items(&[&dashboard, &library, &settings, &wizard, &logs, &updates])?;
    menu.append(&PredefinedMenuItem::separator())?;
    let autostart =
        CheckMenuItem::with_id(id::AUTOSTART, "Start at login", true, autostart_on, None);
    let quit = item(id::QUIT, &m.quit_text, true);
    menu.append_items(&[&autostart, &quit])?;
    Ok(Items {
        menu,
        status,
        stats,
        pause_after,
        pause_now,
        pause_hour,
        pause_tomorrow,
        resume,
        dashboard,
        library,
        updates,
        autostart,
        quit,
    })
}

impl Items {
    /// Update in place; false when the number of lines changed (rebuild instead).
    fn apply(&self, m: &MenuModel, autostart_on: bool) -> bool {
        if self.status.len() != m.status_lines.len() || self.stats.len() != m.stats_lines.len() {
            return false;
        }
        for (item, text) in self.status.iter().zip(&m.status_lines) {
            if item.text() != *text {
                item.set_text(text);
            }
        }
        for (item, text) in self.stats.iter().zip(&m.stats_lines) {
            if item.text() != *text {
                item.set_text(text);
            }
        }
        self.pause_after.set_enabled(m.can_pause_after);
        self.pause_now.set_enabled(m.can_pause_now);
        self.pause_hour.set_enabled(m.can_pause_now);
        self.pause_tomorrow.set_enabled(m.can_pause_now);
        self.resume.set_enabled(m.can_resume);
        self.dashboard.set_enabled(m.can_open_dashboard);
        self.library.set_enabled(m.library_url.is_some());
        if self.updates.text() != m.update_text {
            self.updates.set_text(&m.update_text);
        }
        if self.quit.text() != m.quit_text {
            self.quit.set_text(&m.quit_text);
        }
        if self.autostart.is_checked() != autostart_on {
            self.autostart.set_checked(autostart_on);
        }
        true
    }
}

fn icon(state: IconState) -> Option<Icon> {
    match icons::rgba(state) {
        Ok((rgba, w, h)) => Icon::from_rgba(rgba, w, h).ok(),
        Err(e) => {
            tracing::error!("tray icon {}: {e}", state.name());
            None
        }
    }
}

#[derive(Default)]
struct Native {
    tray: Option<TrayIcon>,
    items: Option<Items>,
    shown: Option<MenuModel>,
}

impl Ui for Native {
    fn show(&mut self, m: &MenuModel, autostart_on: bool) -> anyhow::Result<()> {
        let Some(tray) = &self.tray else {
            let items = build_menu(m, autostart_on)?;
            let mut builder = TrayIconBuilder::new()
                .with_menu(Box::new(items.menu.clone()))
                .with_tooltip(&m.tooltip)
                .with_menu_on_left_click(true);
            if let Some(i) = icon(m.icon) {
                builder = builder.with_icon(i);
            }
            self.tray = Some(builder.build().context("building the tray icon")?);
            self.items = Some(items);
            self.shown = Some(m.clone());
            return Ok(());
        };
        let in_place = self
            .items
            .as_ref()
            .is_some_and(|i| i.apply(m, autostart_on));
        if !in_place {
            let items = build_menu(m, autostart_on)?;
            tray.set_menu(Some(Box::new(items.menu.clone())));
            self.items = Some(items);
        }
        let old = self.shown.as_ref();
        if old.map(|o| o.icon) != Some(m.icon)
            && let Some(i) = icon(m.icon)
        {
            let _ = tray.set_icon(Some(i));
        }
        if old.map(|o| &o.tooltip) != Some(&m.tooltip) {
            let _ = tray.set_tooltip(Some(&m.tooltip));
        }
        self.shown = Some(m.clone());
        Ok(())
    }

    fn hide(&mut self) {
        if let Some(tray) = &self.tray {
            let _ = tray.set_visible(false);
        }
        self.tray = None;
    }
}

pub fn run(setup: Setup) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // A menu-bar agent: no Dock icon even when started outside the .app bundle.
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = p.send_event(UserEvent::Menu(e.id.as_ref().to_string()));
        }));
    }
    let send: super::Sender = {
        let p = std::sync::Mutex::new(proxy);
        Arc::new(move |e| {
            let _ = p.lock().unwrap_or_else(|e| e.into_inner()).send_event(e);
        })
    };
    let (mut app, lock) = App::start(setup, send);
    let mut ui = Native::default();
    event_loop.run(move |event, _, control_flow| {
        let _hold = &lock;
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the loop runs (macOS requirement).
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.refresh_or_fail(&mut ui) {
                    tracing::error!("could not create the tray icon: {e:#}");
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(e) => {
                if app.on_event(&mut ui, e) {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}
