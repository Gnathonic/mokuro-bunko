//! The tray's behaviour, independent of the toolkit: the monitor (instance status), the
//! supervisor and the update check feed a [`MenuModel`]; a menu click comes back as an
//! item id. The toolkit side ([`Ui`]) only shows the model: a StatusNotifierItem over
//! D-Bus on Linux (`sni.rs`, ksni), tray-icon + muda on a tao event loop on Windows and
//! macOS (`native.rs`).

use crate::Options;
use crate::autostart;
use crate::autoupdate::{self, Alarm, Notified};
use crate::client::{Client, PauseRequest};
use crate::discover;
use crate::launch;
use crate::model::{self, InstanceView, MenuModel, UpdateView};
use crate::monitor::{Live, Monitor};
use crate::notify;
use crate::paths::{self, Env, Layout, ProcessEnv};
use crate::status::Status;
use crate::supervise::{self, Launch, Supervisor, World};
use crate::trayconf::TrayConfig;
use crate::updates;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(any(windows, target_os = "macos"))]
mod native;
#[cfg(target_os = "linux")]
mod sni;

pub struct Setup {
    pub opts: Options,
    /// This program (`mokuro-bunko`; on Windows usually the app `mokuro-bunko.exe`).
    pub exe: PathBuf,
    pub layout: Layout,
    /// The `mokuro-bunko` command line the tray starts instances with.
    pub cli: Option<PathBuf>,
    /// Held for the life of the process (one tray per user).
    pub lock: std::fs::File,
}

/// What reaches the tray's loop from its threads and from the toolkit.
#[derive(Debug)]
pub enum UserEvent {
    /// A menu item was clicked (its [`id`]).
    Menu(String),
    /// Something the menu shows may have changed.
    Changed,
    /// The result of "Check for updates".
    Update(UpdateView),
}

/// Sends a [`UserEvent`] to the tray's loop (from any thread).
pub type Sender = Arc<dyn Fn(UserEvent) + Send + Sync>;

/// The menu item ids.
pub mod id {
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

/// The toolkit side: shows a model (creating the icon the first time).
pub trait Ui {
    /// Show `m`; `autostart` is the "Start at login" check mark.
    fn show(&mut self, m: &MenuModel, autostart: bool) -> anyhow::Result<()>;
    /// Remove the icon (Quit).
    fn hide(&mut self);
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
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
    fn managed_instance(&self, role: &str) -> Option<u32> {
        let pid = self.0.live_pid(role)?;
        self.0
            .instances()
            .iter()
            .any(|l| l.control.pid == pid && l.status.as_ref().is_some_and(|s| s.managed))
            .then_some(pid)
    }
}

pub struct App {
    exe: PathBuf,
    layout: Layout,
    cli: Option<PathBuf>,
    child_env: Vec<(String, OsString)>,
    tray_config_path: PathBuf,
    tray_config: Option<TrayConfig>,
    /// tray.json's modification time when last read (a change adds its new roles).
    tray_config_mtime: Option<std::time::SystemTime>,
    /// This tray starts and supervises what tray.json lists (`--no-supervise`: not).
    supervise: bool,
    monitor: Arc<Monitor>,
    supervisor: Option<Supervisor>,
    update: UpdateView,
    send: Sender,
    model: Option<MenuModel>,
    autostart_shown: Option<bool>,
    first_run_checked: bool,
    /// Each role's last status that came from a live answer, and when (restart grace).
    last_good: HashMap<String, (Status, Instant)>,
    /// Which "the update needs you" alarms were already shown.
    notified: Notified,
}

/// Run the tray until Quit (the toolkit of this platform).
pub fn run(setup: Setup) -> anyhow::Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    return native::run(setup);
    #[cfg(target_os = "linux")]
    return sni::run(setup);
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = setup;
        anyhow::bail!("there is no tray on this platform")
    }
}

impl App {
    /// Start the monitor, the supervisor and the redraw ticker; events go to `send`.
    pub fn start(setup: Setup, send: Sender) -> (App, std::fs::File) {
        let Setup {
            opts,
            exe,
            layout,
            cli,
            lock,
        } = setup;
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
            let s = send.clone();
            Arc::new(move || s(UserEvent::Changed))
        };
        let candidates = {
            let layout = layout.clone();
            let path = tray_config_path.clone();
            let extra = opts.storages.clone();
            // tray.json as it is now: a role added while the tray runs is looked for too.
            Box::new(move || {
                let tc = TrayConfig::load(&path).ok().flatten();
                discover::candidate_storages(&ProcessEnv, &layout, tc.as_ref(), &extra)
            })
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
            let s = send.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    s(UserEvent::Changed);
                }
            });
        }
        let tray_config_mtime = mtime(&tray_config_path);
        let app = App {
            exe,
            layout,
            cli,
            child_env,
            supervise: opts.supervise,
            tray_config_mtime,
            tray_config_path,
            tray_config,
            monitor,
            supervisor,
            update: UpdateView::default(),
            send,
            model: None,
            autostart_shown: None,
            first_run_checked: false,
            last_good: HashMap::new(),
            notified: Notified::default(),
        };
        (app, lock)
    }

    /// Handle one event; returns true to exit.
    pub fn on_event(&mut self, ui: &mut dyn Ui, event: UserEvent) -> bool {
        match event {
            UserEvent::Changed => self.refresh(ui),
            UserEvent::Update(v) => {
                self.update = v;
                self.refresh(ui);
            }
            UserEvent::Menu(id) => {
                if self.on_menu(ui, &id) {
                    return true;
                }
                self.refresh(ui);
            }
        }
        false
    }

    pub fn current_model(&mut self) -> MenuModel {
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

    /// Show the current model (first call: create the icon). An error creating the icon
    /// is returned: the tray cannot work without it.
    pub fn refresh_or_fail(&mut self, ui: &mut dyn Ui) -> anyhow::Result<()> {
        self.maybe_first_run();
        self.notify_update_problems();
        let m = self.current_model();
        let autostart_on = autostart::is_enabled(&ProcessEnv);
        if self.model.as_ref() == Some(&m) && self.autostart_shown == Some(autostart_on) {
            return Ok(());
        }
        ui.show(&m, autostart_on)?;
        self.model = Some(m);
        self.autostart_shown = Some(autostart_on);
        Ok(())
    }

    /// tray.json changed while the tray runs (the setup wizard handed it a server or a
    /// processor): supervise the roles it gained. A role it lost keeps running until
    /// Quit (the pages stop it themselves when they take it away).
    fn reload_tray_config(&mut self) {
        let now = mtime(&self.tray_config_path);
        if now == self.tray_config_mtime || !self.supervise {
            return;
        }
        self.tray_config_mtime = now;
        let conf = match TrayConfig::load(&self.tray_config_path) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("{e}");
                return;
            }
        };
        let added = match self.supervisor.as_mut() {
            Some(sup) => sup.add(&conf.managed),
            None if !conf.managed.is_empty() => {
                let send = self.send.clone();
                let sup = Supervisor::start(
                    &conf.managed,
                    |m| m.command_args(&ProcessEnv),
                    Launch {
                        cli: self.cli.clone(),
                        env: self.child_env.clone(),
                        log_dir: self.layout.writable_log_dir(&ProcessEnv),
                    },
                    Arc::new(MonitorWorld(self.monitor.clone())),
                    Arc::new(move || send(UserEvent::Changed)),
                );
                let n = sup.slots.len();
                self.supervisor = Some(sup);
                n
            }
            None => 0,
        };
        if added > 0 {
            tracing::info!(
                "{} changed: now also running {}",
                self.tray_config_path.display(),
                conf.managed
                    .iter()
                    .map(|m| m.role.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.tray_config = Some(conf);
    }

    fn refresh(&mut self, ui: &mut dyn Ui) {
        self.reload_tray_config();
        if let Err(e) = self.refresh_or_fail(ui) {
            tracing::error!("tray: {e:#}");
        }
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
                .set_notice("the mokuro-bunko command line was not found".into());
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
        let send = self.send.clone();
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
            send(UserEvent::Changed);
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

    fn check_updates(&mut self, ui: &mut dyn Ui) {
        if self.update.available || self.model.as_ref().is_some_and(|m| m.update_available) {
            self.open_page("/app/settings#update");
            return;
        }
        let Some(cli) = self.cli.clone() else {
            self.monitor
                .set_notice("the mokuro-bunko command line was not found".into());
            return;
        };
        self.update = UpdateView {
            checking: true,
            ..Default::default()
        };
        self.refresh(ui);
        let env = self.child_env.clone();
        let send = self.send.clone();
        std::thread::spawn(move || {
            let v = updates::check(&cli, &env);
            tracing::info!("update check: {v:?}");
            send(UserEvent::Update(v));
        });
    }

    fn toggle_autostart(&mut self) {
        let env = ProcessEnv;
        let want = !autostart::is_enabled(&env);
        if let Err(e) = autostart::set(&env, &self.exe, want) {
            tracing::error!("start at login: {e}");
            self.monitor.set_notice(format!("start at login: {e}"));
        } else {
            tracing::info!("start at login: {want}");
        }
        // The next refresh shows the check mark as it now is.
        self.autostart_shown = None;
    }

    fn quit(&mut self, ui: &mut dyn Ui) {
        ui.hide();
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
        tracing::info!("quit");
    }

    /// Returns true to exit.
    fn on_menu(&mut self, ui: &mut dyn Ui, id: &str) -> bool {
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
            id::UPDATES => self.check_updates(ui),
            id::AUTOSTART => self.toggle_autostart(),
            id::QUIT => {
                self.quit(ui);
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
