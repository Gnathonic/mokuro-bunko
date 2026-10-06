//! The native side: a tao event loop that owns the tray icon and its muda menu, fed by
//! the monitor (instance status), the supervisor and the update check.

use crate::Options;
use anyhow::{Context, Result};
use bunko_tray::autostart;
use bunko_tray::autoupdate::{self, Alarm, Notified};
use bunko_tray::client::{Client, PauseRequest};
use bunko_tray::discover;
use bunko_tray::icons;
use bunko_tray::launch;
use bunko_tray::model::{self, IconState, InstanceView, MenuModel, UpdateView};
use bunko_tray::monitor::{Live, Monitor};
use bunko_tray::notify;
use bunko_tray::paths::{self, Env, Layout, ProcessEnv};
use bunko_tray::status::Status;
use bunko_tray::supervise::{self, Launch, Supervisor, World};
use bunko_tray::trayconf::TrayConfig;
use bunko_tray::updates;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct Setup {
    pub opts: Options,
    pub exe: PathBuf,
    pub layout: Layout,
    pub cli: Option<PathBuf>,
    /// Held for the life of the process (one tray per user).
    pub lock: std::fs::File,
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Changed,
    Update(UpdateView),
}

mod id {
    pub const PAUSE_AFTER: &str = "pause_after";
    pub const PAUSE_NOW: &str = "pause_now";
    pub const PAUSE_HOUR: &str = "pause_hour";
    pub const PAUSE_TOMORROW: &str = "pause_tomorrow";
    pub const RESUME: &str = "resume";
    pub const DASHBOARD: &str = "dashboard";
    pub const LIBRARY: &str = "library";
    pub const SETTINGS: &str = "settings";
    pub const WIZARD: &str = "wizard";
    pub const LOGS: &str = "logs";
    pub const UPDATES: &str = "updates";
    pub const AUTOSTART: &str = "autostart";
    pub const QUIT: &str = "quit";
}

/// The menu items whose text or state changes.
struct Items {
    menu: Menu,
    status: Vec<MenuItem>,
    stats_menu: Submenu,
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

fn build_menu(m: &MenuModel, autostart_on: bool) -> Result<Items> {
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
        stats_menu,
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
    fn apply(&self, m: &MenuModel) -> bool {
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
        let _ = &self.stats_menu;
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

/// The supervisor's view of the world, backed by the monitor.
struct MonitorWorld(Arc<Monitor>);

impl World for MonitorWorld {
    fn live_instance(&self, role: &str) -> Option<u32> {
        self.0.live_pid(role)
    }
    fn discovered(&self) -> bool {
        self.0.discovered()
    }
    fn service_active(&self, role: &str) -> Option<String> {
        supervise::service_active(role)
    }
}

struct App {
    exe: PathBuf,
    layout: Layout,
    cli: Option<PathBuf>,
    child_env: Vec<(String, OsString)>,
    tray_config_path: PathBuf,
    tray_config: Option<TrayConfig>,
    monitor: Arc<Monitor>,
    supervisor: Option<Supervisor>,
    update: UpdateView,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    items: Option<Items>,
    icon_state: Option<IconState>,
    model: Option<MenuModel>,
    first_run_checked: bool,
    /// Each role's last status that came from a live answer, and when (restart grace).
    last_good: HashMap<String, (Status, Instant)>,
    /// Which "the update needs you" alarms were already shown.
    notified: Notified,
}

pub fn run(setup: Setup) -> Result<()> {
    let Setup {
        opts,
        exe,
        layout,
        cli,
        lock,
    } = setup;
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
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
    }
    let env = ProcessEnv;
    let tray_config_path = layout.tray_config(&env);
    let tray_config = match TrayConfig::load(&tray_config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            None
        }
    };
    let changed: Arc<dyn Fn() + Send + Sync> = {
        let p = proxy.clone();
        Arc::new(move || {
            let _ = p.send_event(UserEvent::Changed);
        })
    };
    let candidates = {
        let layout = layout.clone();
        let tc = tray_config.clone();
        let extra = opts.storages.clone();
        Box::new(move || discover::candidate_storages(&ProcessEnv, &layout, tc.as_ref(), &extra))
    };
    let monitor = Monitor::start(candidates, changed.clone());
    let child_env = layout.child_env(&env);
    let supervisor = match (&tray_config, opts.supervise) {
        (Some(tc), true) if !tc.managed.is_empty() => Some(Supervisor::start(
            &tc.managed,
            |m| m.command_args(&ProcessEnv),
            Launch {
                cli: cli.clone(),
                env: child_env.clone(),
                log_dir: layout.writable_log_dir(&env),
            },
            Arc::new(MonitorWorld(monitor.clone())),
            changed.clone(),
        )),
        _ => None,
    };
    // Re-render every few seconds even without events ("restarting in N s").
    {
        let p = proxy.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if p.send_event(UserEvent::Changed).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App {
        exe,
        layout,
        cli,
        child_env,
        tray_config_path,
        tray_config,
        monitor,
        supervisor,
        update: UpdateView::default(),
        proxy,
        tray: None,
        items: None,
        icon_state: None,
        model: None,
        first_run_checked: false,
        last_good: HashMap::new(),
        notified: Notified::default(),
    };
    let _lock = lock;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the loop runs (macOS requirement).
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.create_tray() {
                    tracing::error!("could not create the tray icon: {e:#}");
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Changed) => app.refresh(),
            Event::UserEvent(UserEvent::Update(v)) => {
                app.update = v;
                app.refresh();
            }
            Event::UserEvent(UserEvent::Menu(e)) if app.on_menu(e.id.as_ref()) => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

impl App {
    fn current_model(&mut self) -> MenuModel {
        let lives = self.monitor.instances();
        let now = Instant::now();
        // Remember each role's last answer; a role that stops answering during its own
        // update restart keeps showing "Updating to X…" for a few minutes.
        for l in &lives {
            if let (Some(s), None) = (&l.status, &l.error) {
                self.last_good
                    .insert(l.control.role.clone(), (s.clone(), now));
            }
        }
        let grace = |last_good: &HashMap<String, (Status, Instant)>, role: &str| {
            last_good
                .get(role)
                .and_then(|(s, at)| autoupdate::restart_grace(Some(s), now.duration_since(*at)))
        };
        let mut updating: Vec<(String, Option<String>)> = Vec::new();
        let mut roles: Vec<&str> = lives.iter().map(|l| l.control.role.as_str()).collect();
        let slot_roles: Vec<String> = self
            .supervisor
            .as_ref()
            .map(|s| s.slots.iter().map(|x| x.managed.role.clone()).collect())
            .unwrap_or_default();
        roles.extend(slot_roles.iter().map(String::as_str));
        roles.extend(self.last_good.keys().map(String::as_str));
        roles.sort_unstable();
        roles.dedup();
        for role in roles {
            let answering = lives
                .iter()
                .any(|l| l.control.role == role && l.error.is_none() && l.status.is_some());
            if !answering && let Some(v) = grace(&self.last_good, role) {
                updating.push((role.to_string(), v));
            }
        }
        let instances: Vec<InstanceView> = lives
            .iter()
            .map(|l| {
                let mut status = l.status.clone();
                // An old status that said "restarting" is only trusted for the grace.
                if l.error.is_some()
                    && status
                        .as_ref()
                        .and_then(|s| s.update.as_ref())
                        .is_some_and(|u| autoupdate::is_restarting(&u.state))
                {
                    status = None;
                }
                InstanceView {
                    role: l.control.role.clone(),
                    status,
                    error: l.error.clone(),
                    tray_started: self
                        .supervisor
                        .as_ref()
                        .is_some_and(|s| s.is_child(l.control.pid)),
                }
            })
            .collect();
        let visible: Vec<&str> = lives.iter().map(|l| l.control.role.as_str()).collect();
        let supervised = self
            .supervisor
            .as_ref()
            .map(|s| s.views(&visible))
            .unwrap_or_default();
        let notice = self.monitor.notice();
        model::build(&model::Inputs {
            instances: &instances,
            supervised: &supervised,
            update: &self.update,
            updating: &updating,
            notice: notice.as_deref(),
            now: chrono::Local::now(),
        })
    }

    /// One desktop notification per distinct "the update needs you" problem. These are
    /// alarms: they ignore `tray.json`'s `notifications` switch.
    fn notify_update_problems(&mut self) {
        for l in self.monitor.instances() {
            let (Some(s), None) = (&l.status, &l.error) else {
                continue;
            };
            for a in self.notified.observe(&l.control.role, s) {
                tracing::warn!("update needs the owner ({}): {}", a.role, a.text);
                notify::alarm(Alarm::TITLE, &a.body());
            }
        }
    }

    fn create_tray(&mut self) -> Result<()> {
        let m = self.current_model();
        let items = build_menu(&m, autostart::is_enabled(&ProcessEnv))?;
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(items.menu.clone()))
            .with_tooltip(&m.tooltip)
            .with_menu_on_left_click(true);
        if let Some(i) = icon(m.icon) {
            builder = builder.with_icon(i);
        }
        let tray = builder.build().context("building the tray icon")?;
        self.icon_state = Some(m.icon);
        self.items = Some(items);
        self.tray = Some(tray);
        self.model = Some(m);
        Ok(())
    }

    fn refresh(&mut self) {
        self.maybe_first_run();
        self.notify_update_problems();
        if self.tray.is_none() {
            return;
        }
        let m = self.current_model();
        let Some(tray) = &self.tray else { return };
        if self.model.as_ref() == Some(&m) {
            return;
        }
        let in_place = self.items.as_ref().is_some_and(|i| i.apply(&m));
        if !in_place {
            match build_menu(&m, autostart::is_enabled(&ProcessEnv)) {
                Ok(items) => {
                    tray.set_menu(Some(Box::new(items.menu.clone())));
                    self.items = Some(items);
                }
                Err(e) => tracing::error!("menu: {e}"),
            }
        }
        if self.icon_state != Some(m.icon) {
            if let Some(i) = icon(m.icon) {
                let _ = tray.set_icon(Some(i));
            }
            self.icon_state = Some(m.icon);
        }
        if self.model.as_ref().map(|o| &o.tooltip) != Some(&m.tooltip) {
            let _ = tray.set_tooltip(Some(&m.tooltip));
        }
        self.model = Some(m);
    }

    /// Nothing configured and nothing running: open the setup wizard once.
    fn maybe_first_run(&mut self) {
        if self.first_run_checked || !self.monitor.discovered() {
            return;
        }
        self.first_run_checked = true;
        let env = ProcessEnv;
        let configured = self.tray_config.is_some()
            || self.tray_config_path.exists()
            || env.path("MOKURO_CONFIG").is_some_and(|p| p.exists())
            || paths::server_default_config(&env).exists()
            || self
                .layout
                .portable_data
                .as_ref()
                .is_some_and(|d| d.join("config.yaml").exists())
            || discover::processor_configs(&env, None)
                .iter()
                .any(|p| p.exists());
        if !configured && self.monitor.instances().is_empty() {
            tracing::info!("nothing is set up on this machine yet: starting the setup wizard");
            self.spawn_gui("/app/");
        }
    }

    /// `mokuro-bunko gui --open <next>`: the app pages with nothing running; it opens
    /// the browser itself.
    fn spawn_gui(&self, next: &str) {
        let Some(cli) = &self.cli else {
            self.monitor
                .set_notice("the mokuro-bunko program was not found next to the tray".into());
            return;
        };
        let mut cmd = std::process::Command::new(cli);
        cmd.args(["gui", "--open", next])
            .envs(self.child_env.iter().map(|(k, v)| (k, v)));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let monitor = self.monitor.clone();
        let watched = launch::spawn_watched(cmd, launch::EARLY, move |e| {
            report_failure(&monitor, "Couldn't open the setup app", &e);
        });
        match watched {
            Ok(pid) => tracing::info!("started `mokuro-bunko gui` (pid {pid})"),
            Err(e) => self
                .monitor
                .set_notice(format!("could not start mokuro-bunko gui: {e}")),
        }
    }

    /// The instance whose pages to open: library server, then processor, then setup.
    fn page_host(&self) -> Option<Live> {
        let lives = self.monitor.instances();
        ["server", "processor", "gui"].iter().find_map(|role| {
            lives
                .iter()
                .find(|l| l.control.role == *role && l.status.is_some())
                .cloned()
        })
    }

    /// Open `next` on the page host, signed in with a fresh single-use code (asked for
    /// off the UI thread).
    fn open_page(&self, next: &str) {
        match self.page_host() {
            Some(live) => {
                tracing::info!(
                    "opening {next} on the {} instance (pid {})",
                    live.control.role,
                    live.control.pid
                );
                let monitor = self.monitor.clone();
                let next = next.to_string();
                std::thread::spawn(
                    move || match Client::new(&live.control).sign_in_url(&next) {
                        Ok(url) => open_url(&url, &monitor),
                        Err(e) => {
                            tracing::error!("sign-in code from {}: {e}", live.control.role);
                            monitor.set_notice(format!("could not open the page: {e}"));
                        }
                    },
                );
            }
            None => self.spawn_gui(next),
        }
    }

    fn pause_all(&self, req: Option<PauseRequest>) {
        let targets: Vec<Live> = self
            .monitor
            .instances()
            .into_iter()
            .filter(|l| l.status.as_ref().is_some_and(|s| s.can_pause()))
            .collect();
        let monitor = self.monitor.clone();
        std::thread::spawn(move || {
            for live in targets {
                let client = Client::new(&live.control);
                let res = match &req {
                    Some(r) => client.pause(r),
                    None => client.resume(),
                };
                let what = if req.is_some() { "pause" } else { "resume" };
                match res {
                    Ok(status) => {
                        tracing::info!(
                            "{what} {} (pid {}): now {}",
                            live.control.role,
                            live.control.pid,
                            status.state
                        );
                        monitor.record(live.control.pid, status);
                    }
                    Err(e) => {
                        tracing::error!("{what} {}: {e}", live.control.role);
                        monitor.set_notice(format!("{what} failed: {e}"));
                    }
                }
            }
        });
    }

    fn show_logs(&self) {
        let dir = self
            .monitor
            .instances()
            .iter()
            .find_map(|l| l.status.as_ref().and_then(|s| s.urls.logs_dir.clone()))
            .map(PathBuf::from)
            .unwrap_or_else(|| self.layout.writable_log_dir(&ProcessEnv));
        let _ = std::fs::create_dir_all(&dir);
        open_path(&dir);
    }

    fn check_updates(&mut self) {
        if self.update.available {
            self.open_page("/app/settings#update");
            return;
        }
        let Some(cli) = self.cli.clone() else {
            self.monitor
                .set_notice("the mokuro-bunko program was not found next to the tray".into());
            return;
        };
        self.update = UpdateView {
            checking: true,
            ..Default::default()
        };
        self.refresh();
        let env = self.child_env.clone();
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let v = updates::check(&cli, &env);
            tracing::info!("update check: {v:?}");
            let _ = p.send_event(UserEvent::Update(v));
        });
    }

    fn toggle_autostart(&self) {
        let env = ProcessEnv;
        let want = !autostart::is_enabled(&env);
        if let Err(e) = autostart::set(&env, &self.exe, want) {
            tracing::error!("start at login: {e}");
            self.monitor.set_notice(format!("start at login: {e}"));
        } else {
            tracing::info!("start at login: {want}");
        }
        if let Some(items) = &self.items {
            items.autostart.set_checked(autostart::is_enabled(&env));
        }
    }

    fn quit(&mut self) {
        if let Some(tray) = &self.tray {
            let _ = tray.set_visible(false);
        }
        if let Some(sup) = &self.supervisor {
            let monitor = self.monitor.clone();
            sup.stop_all(
                |pid| match monitor.client_for_pid(pid) {
                    Some(c) => c.stop().is_ok(),
                    None => false,
                },
                Duration::from_secs(30),
            );
        }
        self.monitor.stop();
        self.tray = None;
        tracing::info!("quit");
    }

    /// Returns true to exit.
    fn on_menu(&mut self, id: &str) -> bool {
        tracing::info!("menu: {id}");
        let now = chrono::Local::now();
        match id {
            id::PAUSE_AFTER => self.pause_all(Some(PauseRequest {
                mode: "after_volume",
                until: None,
            })),
            id::PAUSE_NOW => self.pause_all(Some(PauseRequest {
                mode: "now",
                until: None,
            })),
            id::PAUSE_HOUR => self.pause_all(Some(PauseRequest {
                mode: "now",
                until: Some(model::in_one_hour(now)),
            })),
            id::PAUSE_TOMORROW => self.pause_all(Some(PauseRequest {
                mode: "now",
                until: Some(model::tomorrow_at_8(now)),
            })),
            id::RESUME => self.pause_all(None),
            id::DASHBOARD => {
                let next = self
                    .page_host()
                    .and_then(|l| l.status.and_then(|s| s.urls.dashboard))
                    .unwrap_or_else(|| "/app/dashboard".into());
                self.open_page(&next);
            }
            id::LIBRARY => {
                if let Some(url) = self.model.as_ref().and_then(|m| m.library_url.clone()) {
                    open_url(&url, &self.monitor);
                }
            }
            id::SETTINGS => self.open_page("/app/settings"),
            id::WIZARD => self.open_page("/app/setup"),
            id::LOGS => self.show_logs(),
            id::UPDATES => self.check_updates(),
            id::AUTOSTART => self.toggle_autostart(),
            id::QUIT => {
                self.quit();
                return true;
            }
            other => tracing::debug!("menu event {other}"),
        }
        false
    }
}

/// A helper that failed right after a menu click: logged, shown as the menu notice,
/// and on macOS as a notification.
fn report_failure(monitor: &Monitor, what: &str, e: &launch::EarlyExit) {
    tracing::error!("{what}: {} ({})", e.reason(), e.status);
    // A menu item is one line: keep it short, the log has the whole message.
    let mut reason = e.reason();
    if reason.chars().count() > 90 {
        reason = reason.chars().take(89).collect::<String>() + "…";
    }
    monitor.set_notice(format!("{what}: {reason} (see logs)"));
    launch::notify("Mokuro Bunko", &format!("{what}: {}", e.reason()));
}

fn open_url(url: &str, monitor: &Arc<Monitor>) {
    // Never log the URL itself: it carries a sign-in code.
    #[cfg(unix)]
    {
        // The desktop's opener (`open` on macOS, xdg-open & co. on Linux), watched so
        // that a failure is shown instead of nothing happening.
        // macOS: `open` from PATH, as `mokuro-bunko gui` does (the `open` crate runs
        // /usr/bin/open by absolute path, which a test's stand-in cannot catch).
        let commands = if cfg!(target_os = "macos") {
            let mut c = std::process::Command::new("open");
            c.arg(url);
            vec![c]
        } else {
            open::commands(url)
        };
        for cmd in commands {
            let m = monitor.clone();
            match launch::spawn_watched(cmd, launch::EARLY, move |e| {
                report_failure(&m, "Couldn't open the browser", &e)
            }) {
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::error!("could not open the browser: {e}");
                    monitor.set_notice(format!("Couldn't open the browser: {e}"));
                    return;
                }
            }
        }
        monitor.set_notice("Couldn't open the browser: no opener found (xdg-open)".into());
    }
    #[cfg(not(unix))]
    if let Err(e) = open::that_detached(url) {
        tracing::error!("could not open the browser: {e}");
        monitor.set_notice(format!("Couldn't open the browser: {e}"));
    }
}

fn open_path(path: &Path) {
    if let Err(e) = open::that_detached(path) {
        tracing::error!("could not open {}: {e}", path.display());
    }
}
