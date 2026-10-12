//! Linux: can a tray icon show in this desktop session? The tray is a
//! StatusNotifierItem: it needs the session D-Bus and a StatusNotifier host (the panel's
//! tray: KDE Plasma, Xfce, LXQt, Cinnamon, ...; on GNOME the "AppIndicator and
//! KStatusNotifierItem Support" extension). `mokuro-bunko doctor` reports it.

use std::time::Duration;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// A watcher with a registered host: the icon shows.
    Ready,
    /// No session D-Bus to talk to (why).
    NoBus(String),
    /// The bus is there, but nothing provides `org.kde.StatusNotifierWatcher`.
    NoWatcher,
    /// A watcher, but no host has registered with it (no panel shows the icons).
    NoHost,
}

/// Ask the session bus (2 s at most).
pub fn host() -> Host {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return Host::NoBus(e.to_string()),
    };
    rt.block_on(async {
        match tokio::time::timeout(Duration::from_secs(2), ask()).await {
            Ok(h) => h,
            Err(_) => Host::NoBus("the session bus did not answer".into()),
        }
    })
}

async fn ask() -> Host {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => return Host::NoBus(e.to_string()),
    };
    let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return Host::NoBus(e.to_string()),
    };
    let name = match zbus::names::BusName::try_from(WATCHER) {
        Ok(n) => n,
        Err(e) => return Host::NoBus(e.to_string()),
    };
    if !dbus.name_has_owner(name).await.unwrap_or(false) {
        return Host::NoWatcher;
    }
    let props = match zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(WATCHER)
        .and_then(|b| b.path("/StatusNotifierWatcher"))
    {
        Ok(b) => b.build().await,
        Err(e) => return Host::NoBus(e.to_string()),
    };
    let Ok(props) = props else {
        return Host::NoHost;
    };
    let iface = zbus::names::InterfaceName::from_static_str_unchecked(WATCHER);
    match props.get(iface, "IsStatusNotifierHostRegistered").await {
        Ok(v) if bool::try_from(&*v).unwrap_or(false) => Host::Ready,
        Ok(_) => Host::NoHost,
        // A watcher without the property: assume a host (some implementations).
        Err(_) => Host::Ready,
    }
}
