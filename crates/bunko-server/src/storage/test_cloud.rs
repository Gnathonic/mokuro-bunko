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
//!
//! They take effect only in a build with the `testing` feature. A build without it
//! refuses to start while one is set ([`refuse_if_set`], from `Services::new`), so a
//! stray variable can never send a real server's tokens somewhere else. The variables
//! are read each time a store or the connect probe is built (a test changes them
//! between servers).

/// Every variable, in the order they are checked.
pub const VARS: [&str; 5] = [
    "MOKURO_TEST_CLOUD_API_BASE_GDRIVE",
    "MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE",
    "MOKURO_TEST_CLOUD_API_BASE_MEGA",
    "MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE",
    "MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT",
];

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
        return Ok(());
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
