//! Test-only settings that point the cloud drivers at a simulator (`bunko-cloudsim`)
//! instead of Google, Microsoft and MEGA, so a whole server can run against it:
//!
//! | Variable | Replaces |
//! |---|---|
//! | `MOKURO_TEST_CLOUD_API_BASE_GDRIVE` | `GDriveConfig.api_base` (`https://www.googleapis.com`) |
//! | `MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE` | `OneDriveConfig.api_base` (`https://graph.microsoft.com/v1.0`) |
//! | `MOKURO_TEST_CLOUD_API_BASE_MEGA` | `ClientConfig.api_origin` (`https://g.api.mega.co.nz/`) |
//! | `MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE` | the Google client's `token_url` |
//! | `MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT` | the Microsoft client's `token_url` |
//! | `MOKURO_TEST_CLOUD_TUNE` | the store's lease and retry timing ([`StoreTune`]) |
//!
//! They take effect only in a build with the `testing` feature. A build without it
//! refuses to start while one is set ([`refuse_if_set`], from `Services::new`), so a
//! stray variable can never send a real server's tokens somewhere else. The variables
//! are read each time a store or the connect probe is built (a test changes them
//! between servers).

/// Every variable, in the order they are checked.
pub const VARS: [&str; 6] = [
    "MOKURO_TEST_CLOUD_API_BASE_GDRIVE",
    "MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE",
    "MOKURO_TEST_CLOUD_API_BASE_MEGA",
    "MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE",
    "MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT",
    TUNE_VAR,
];

/// The store timing of a server the rig spawns ([`StoreTune`]).
pub const TUNE_VAR: &str = "MOKURO_TEST_CLOUD_TUNE";

/// The addresses to use instead of the providers' own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub gdrive_api: Option<String>,
    pub onedrive_api: Option<String>,
    pub mega_origin: Option<String>,
    pub google_token_url: Option<String>,
    pub microsoft_token_url: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Overrides {
    /// From a variable lookup (empty values count as unset).
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Overrides {
        let get = |n: &str| get(n).filter(|v| !v.trim().is_empty());
        Overrides {
            gdrive_api: get(VARS[0]),
            onedrive_api: get(VARS[1]),
            mega_origin: get(VARS[2]),
            google_token_url: get(VARS[3]),
            microsoft_token_url: get(VARS[4]),
        }
    }

    /// The process environment's overrides; none at all without `testing`.
    pub fn current() -> Overrides {
        if cfg!(feature = "testing") {
            Overrides::from_vars(env)
        } else {
            Overrides::default()
        }
    }

    pub fn apply_gdrive(&self, cfg: &mut bunko_remote::gdrive::GDriveConfig) {
        if let Some(b) = &self.gdrive_api {
            cfg.api_base = b.clone();
        }
    }

    pub fn apply_onedrive(&self, cfg: &mut bunko_remote::onedrive::OneDriveConfig) {
        if let Some(b) = &self.onedrive_api {
            cfg.api_base = b.clone();
        }
    }

    pub fn apply_mega(&self, cfg: &mut bunko_remote::mega::MegaConfig) {
        if let Some(b) = &self.mega_origin {
            cfg.client.api_origin = b.clone();
        }
    }

    pub fn apply_oauth(&self, client: &mut bunko_remote::auth::OAuthClient) {
        use bunko_remote::auth::Provider;
        let url = match client.provider {
            Provider::Google => &self.google_token_url,
            Provider::Microsoft => &self.microsoft_token_url,
        };
        if let Some(u) = url {
            client.token_url = u.clone();
        }
    }
}

/// The startup check, pure: `Err` naming the first variable set when the build has no
/// test hooks.
pub fn check(testing: bool, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if testing {
        // A tune the server cannot read would quietly run on the real timing.
        return match get(TUNE_VAR).filter(|v| !v.trim().is_empty()) {
            Some(v) => StoreTune::parse(&v)
                .map(|_| ())
                .map_err(|e| format!("{TUNE_VAR}: {e}")),
            None => Ok(()),
        };
    }
    match VARS
        .into_iter()
        .find(|v| get(v).is_some_and(|x| !x.trim().is_empty()))
    {
        Some(v) => Err(format!(
            "{v} is set, but this build has no test hooks (the `testing` feature); unset it to start"
        )),
        None => Ok(()),
    }
}

/// Refuse to start a build without `testing` while a test-cloud variable is set.
pub fn refuse_if_set() -> Result<(), String> {
    check(cfg!(feature = "testing"), env)
}

/// `MOKURO_TEST_CLOUD_TUNE`: the lease and retry timing of a store in a server started
/// as its own process, which the in-process [`register_tune`] cannot reach (the
/// interop rig's lease handover cannot wait out a 60 s settle and a 15 min ttl).
/// Comma-separated `key=value` pairs: `lease` (`on`/`off`), `lease_ttl_s`,
/// `lease_settle_ms`, `retry_base_ms`, `queue_floor_ms`; a key left out keeps the
/// store's own setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreTune {
    pub lease: Option<bool>,
    pub lease_ttl: Option<std::time::Duration>,
    pub lease_settle: Option<std::time::Duration>,
    pub retry_base: Option<std::time::Duration>,
    pub queue_floor: Option<std::time::Duration>,
}

impl StoreTune {
    pub fn parse(text: &str) -> Result<StoreTune, String> {
        use std::time::Duration;
        let mut t = StoreTune::default();
        for pair in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("`{pair}` is not key=value"))?;
            let (key, value) = (key.trim(), value.trim());
            let n = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{key}: `{value}` is not a whole number"))
            };
            match key {
                "lease" => {
                    t.lease = Some(match value {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("lease: `{value}` is not on or off")),
                    })
                }
                "lease_ttl_s" => t.lease_ttl = Some(Duration::from_secs(n()?)),
                "lease_settle_ms" => t.lease_settle = Some(Duration::from_millis(n()?)),
                "retry_base_ms" => t.retry_base = Some(Duration::from_millis(n()?)),
                "queue_floor_ms" => t.queue_floor = Some(Duration::from_millis(n()?)),
                _ => return Err(format!("unknown key `{key}`")),
            }
        }
        Ok(t)
    }

    /// The process environment's tune; none at all without `testing`.
    pub fn current() -> StoreTune {
        if !cfg!(feature = "testing") {
            return StoreTune::default();
        }
        // `refuse_if_set` already stopped a server whose tune does not parse.
        env(TUNE_VAR)
            .and_then(|v| StoreTune::parse(&v).ok())
            .unwrap_or_default()
    }

    pub fn apply(&self, rc: &mut bunko_remote::RemoteStoreConfig) {
        if let Some(on) = self.lease {
            rc.lease = on;
        }
        if self.lease_ttl.is_some() {
            rc.lease_ttl = self.lease_ttl;
        }
        if self.lease_settle.is_some() {
            rc.lease_settle = self.lease_settle;
        }
        if let Some(d) = self.retry_base {
            rc.retry_base = d;
        }
        if self.queue_floor.is_some() {
            rc.queue_floor = self.queue_floor;
        }
    }
}

#[cfg(feature = "testing")]
mod tune {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use parking_lot::Mutex;

    use crate::storage::testing::Tune;

    static TUNES: Mutex<Option<HashMap<PathBuf, Tune>>> = Mutex::new(None);

    /// The next cloud stores opened over the storage base path `base` get `tune` applied
    /// to their settings (a test's lease and retry timing), until [`clear`].
    pub fn register(base: &Path, tune: Tune) {
        TUNES
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(base.to_path_buf(), tune);
    }

    pub fn clear(base: &Path) {
        if let Some(m) = TUNES.lock().as_mut() {
            m.remove(base);
        }
    }

    pub fn apply(base: &Path, rc: &mut bunko_remote::RemoteStoreConfig) {
        let tune = TUNES.lock().as_ref().and_then(|m| m.get(base).copied());
        if let Some(t) = tune {
            t(rc);
        }
    }
}

#[cfg(feature = "testing")]
pub use tune::{apply as apply_tune, clear as clear_tune, register as register_tune};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn a_build_without_test_hooks_refuses_a_set_variable_by_name() {
        assert_eq!(check(false, vars(&[])), Ok(()));
        let err = check(
            false,
            vars(&[(
                "MOKURO_TEST_CLOUD_API_BASE_MEGA",
                "http://127.0.0.1:1/mega/",
            )]),
        )
        .unwrap_err();
        assert!(
            err.starts_with("MOKURO_TEST_CLOUD_API_BASE_MEGA is set"),
            "{err}"
        );
        // The first one set, in order; blank values are unset.
        let err = check(
            false,
            vars(&[
                ("MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT", "x"),
                ("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", "y"),
                ("MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE", " "),
            ]),
        )
        .unwrap_err();
        assert!(
            err.starts_with("MOKURO_TEST_CLOUD_API_BASE_GDRIVE"),
            "{err}"
        );
        assert_eq!(
            check(false, vars(&[("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", "")])),
            Ok(())
        );
        // With the hooks built in, the variables are allowed.
        assert_eq!(
            check(true, vars(&[("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", "y")])),
            Ok(())
        );
    }

    #[test]
    fn a_store_tune_sets_the_lease_and_retry_timing() {
        let t = StoreTune::parse(
            "lease_ttl_s=12, lease_settle_ms=500,retry_base_ms=50,queue_floor_ms=200,lease=off",
        )
        .unwrap();
        let mut rc = bunko_remote::RemoteStoreConfig::new("/tmp/r", "/tmp/b/cloud/x", "f");
        t.apply(&mut rc);
        assert!(!rc.lease);
        assert_eq!(rc.lease_ttl, Some(std::time::Duration::from_secs(12)));
        assert_eq!(rc.lease_settle, Some(std::time::Duration::from_millis(500)));
        assert_eq!(rc.retry_base, std::time::Duration::from_millis(50));
        assert_eq!(rc.queue_floor, Some(std::time::Duration::from_millis(200)));
        // Unset keys keep the store's own settings.
        let mut rc = bunko_remote::RemoteStoreConfig::new("/tmp/r", "/tmp/b/cloud/x", "f");
        StoreTune::parse("lease_ttl_s=30").unwrap().apply(&mut rc);
        assert!(rc.lease);
        assert_eq!(rc.lease_settle, None);
        assert_eq!(StoreTune::parse(" ").unwrap(), StoreTune::default());
    }

    #[test]
    fn a_tune_the_server_cannot_read_stops_it_starting() {
        for bad in ["lease_ttl_s", "lease_ttl_s=soon", "lease=maybe", "ttl=3"] {
            let err = check(true, vars(&[(TUNE_VAR, bad)])).unwrap_err();
            assert!(err.starts_with("MOKURO_TEST_CLOUD_TUNE: "), "{bad}: {err}");
        }
        assert_eq!(check(true, vars(&[(TUNE_VAR, "lease_ttl_s=9")])), Ok(()));
        // Like the addresses, refused outright by a build without the hooks.
        let err = check(false, vars(&[(TUNE_VAR, "lease_ttl_s=9")])).unwrap_err();
        assert!(err.starts_with("MOKURO_TEST_CLOUD_TUNE is set"), "{err}");
    }

    #[test]
    fn overrides_replace_each_address() {
        let o = Overrides::from_vars(vars(&[
            ("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", "http://sim/google"),
            (
                "MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE",
                "http://sim/graph/v1.0",
            ),
            ("MOKURO_TEST_CLOUD_API_BASE_MEGA", "http://sim/mega/"),
            (
                "MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE",
                "http://sim/oauth/google/token",
            ),
            (
                "MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT",
                "http://sim/oauth/microsoft/token",
            ),
        ]));
        let mut g = bunko_remote::gdrive::GDriveConfig::new("/tmp/x");
        o.apply_gdrive(&mut g);
        assert_eq!(g.api_base, "http://sim/google");
        let mut od = bunko_remote::onedrive::OneDriveConfig::default();
        o.apply_onedrive(&mut od);
        assert_eq!(od.api_base, "http://sim/graph/v1.0");
        let mut m = bunko_remote::mega::MegaConfig::new("/tmp/x");
        o.apply_mega(&mut m);
        assert_eq!(m.client.api_origin, "http://sim/mega/");
        let mut gc = bunko_remote::auth::OAuthClient::google("cid", "secret");
        o.apply_oauth(&mut gc);
        assert_eq!(gc.token_url, "http://sim/oauth/google/token");
        let mut mc = bunko_remote::auth::OAuthClient::microsoft("cid");
        o.apply_oauth(&mut mc);
        assert_eq!(mc.token_url, "http://sim/oauth/microsoft/token");
        // Nothing set: the providers' own addresses stay.
        let none = Overrides::from_vars(vars(&[]));
        let mut g = bunko_remote::gdrive::GDriveConfig::new("/tmp/x");
        none.apply_gdrive(&mut g);
        assert_eq!(g.api_base, bunko_remote::gdrive::DEFAULT_API_BASE);
    }
}
