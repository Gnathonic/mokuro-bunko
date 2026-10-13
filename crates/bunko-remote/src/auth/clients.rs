//! The OAuth clients embedded in every build (§7, D25).
//!
//! Google installed-app ("Desktop") and "TVs and Limited Input" client secrets are
//! public by design: Google documents that installed apps cannot keep secrets, so
//! the secret is not confidential and ships in the binary (prov-gdrive §A). They are
//! constants here and are never read from the environment at build time.
//!
//! Pairings: `release` = project `mokuro-reader-414418` (reader.mokuro.app), `beta`
//! = `mokuro-reader-tan`. The local-dev project (`mokuro-reader-tan-dev`) is not
//! embedded; it is reachable only through `storage.library.oauth.google` overrides
//! (pairing `custom`).

use bunko_core::config::{GoogleOAuthOverrides, GoogleProject, MicrosoftOAuthOverrides};

use super::oauth::OAuthClient;

/// `mokuro-reader-414418` (release), Desktop client: the loopback flow.
pub const RELEASE_DESKTOP_CLIENT_ID: &str =
    "668077722590-5u0698aafve2udlcru7i184qrb7m59jv.apps.googleusercontent.com";
pub const RELEASE_DESKTOP_CLIENT_SECRET: &str = "REDACTED-GOOGLE-CLIENT-SECRET";
/// `mokuro-reader-414418` (release), TVs and Limited Input client: the device flow.
pub const RELEASE_DEVICE_CLIENT_ID: &str =
    "668077722590-9pni7na6bv58fgnacdauenfhmg1d58g0.apps.googleusercontent.com";
pub const RELEASE_DEVICE_CLIENT_SECRET: &str = "REDACTED-GOOGLE-CLIENT-SECRET";

/// `mokuro-reader-tan` (beta), Desktop client.
pub const BETA_DESKTOP_CLIENT_ID: &str =
    "868461116957-anvbst1eiq2pbegk578f280mhj3umrbv.apps.googleusercontent.com";
pub const BETA_DESKTOP_CLIENT_SECRET: &str = "REDACTED-GOOGLE-CLIENT-SECRET";
/// `mokuro-reader-tan` (beta), TVs and Limited Input client.
pub const BETA_DEVICE_CLIENT_ID: &str =
    "868461116957-b494nfd65jv26p3ii067kkeihvi56m8l.apps.googleusercontent.com";
pub const BETA_DEVICE_CLIENT_SECRET: &str = "REDACTED-GOOGLE-CLIENT-SECRET";

/// Bunko's Microsoft registration: a public client (no secret), native redirect
/// `http://localhost/bunko` (D17).
pub const MICROSOFT_CLIENT_ID: &str = "50f6e3ce-aa4f-4a0e-8357-7e51a6aad04d";

/// The Google clients of one pairing, by flow. A flow whose client is missing (a
/// partial `custom` override) is `None` and cannot run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleClients {
    pub loopback: Option<OAuthClient>,
    pub device: Option<OAuthClient>,
}

/// The Google clients for a pairing. Any `storage.library.oauth.google` override
/// replaces both embedded clients (pairing `custom`, as config normalisation sets it),
/// whatever `pairing` says; each flow then needs its own id and secret.
pub fn google_clients(pairing: GoogleProject, overrides: &GoogleOAuthOverrides) -> GoogleClients {
    if overrides.any() || pairing == GoogleProject::Custom {
        let pair = |id: &Option<String>, secret: &Option<String>| match (id, secret) {
            (Some(id), Some(secret)) => Some(OAuthClient::google(id, secret)),
            _ => None,
        };
        return GoogleClients {
            loopback: pair(&overrides.client_id, &overrides.client_secret),
            device: pair(&overrides.device_client_id, &overrides.device_client_secret),
        };
    }
    let (desktop, desktop_secret, device, device_secret) = match pairing {
        GoogleProject::Beta => (
            BETA_DESKTOP_CLIENT_ID,
            BETA_DESKTOP_CLIENT_SECRET,
            BETA_DEVICE_CLIENT_ID,
            BETA_DEVICE_CLIENT_SECRET,
        ),
        _ => (
            RELEASE_DESKTOP_CLIENT_ID,
            RELEASE_DESKTOP_CLIENT_SECRET,
            RELEASE_DEVICE_CLIENT_ID,
            RELEASE_DEVICE_CLIENT_SECRET,
        ),
    };
    GoogleClients {
        loopback: Some(OAuthClient::google(desktop, desktop_secret)),
        device: Some(OAuthClient::google(device, device_secret)),
    }
}

/// Bunko's Microsoft client, or the `storage.library.oauth.microsoft.client_id`
/// override. One public client serves both flows.
pub fn microsoft_client(overrides: &MicrosoftOAuthOverrides) -> OAuthClient {
    OAuthClient::microsoft(
        overrides
            .client_id
            .as_deref()
            .unwrap_or(MICROSOFT_CLIENT_ID),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::{GOOGLE_SCOPE, Provider};

    fn ids(c: &GoogleClients) -> (Option<&str>, Option<&str>) {
        (
            c.loopback.as_ref().map(|c| c.client_id.as_str()),
            c.device.as_ref().map(|c| c.client_id.as_str()),
        )
    }

    #[test]
    fn release_and_beta_use_their_projects() {
        let none = GoogleOAuthOverrides::default();
        let r = google_clients(GoogleProject::Release, &none);
        assert_eq!(
            ids(&r),
            (
                Some(RELEASE_DESKTOP_CLIENT_ID),
                Some(RELEASE_DEVICE_CLIENT_ID)
            )
        );
        assert!(RELEASE_DESKTOP_CLIENT_ID.starts_with("668077722590-"));
        assert!(RELEASE_DEVICE_CLIENT_ID.starts_with("668077722590-"));
        let lb = r.loopback.unwrap();
        assert_eq!(lb.provider, Provider::Google);
        assert_eq!(
            lb.client_secret.as_deref(),
            Some(RELEASE_DESKTOP_CLIENT_SECRET)
        );
        assert_eq!(lb.scopes, vec![GOOGLE_SCOPE.to_string()]);
        assert_eq!(
            r.device.unwrap().client_secret.as_deref(),
            Some(RELEASE_DEVICE_CLIENT_SECRET)
        );

        let b = google_clients(GoogleProject::Beta, &none);
        assert_eq!(
            ids(&b),
            (Some(BETA_DESKTOP_CLIENT_ID), Some(BETA_DEVICE_CLIENT_ID))
        );
        assert!(BETA_DESKTOP_CLIENT_ID.starts_with("868461116957-"));
        assert_eq!(
            b.loopback.unwrap().client_secret.as_deref(),
            Some(BETA_DESKTOP_CLIENT_SECRET)
        );
    }

    #[test]
    fn overrides_replace_both_clients() {
        let full = GoogleOAuthOverrides {
            client_id: Some("dev-desktop".into()),
            client_secret: Some("dev-desktop-secret".into()),
            device_client_id: Some("dev-device".into()),
            device_client_secret: Some("dev-device-secret".into()),
        };
        let c = google_clients(GoogleProject::Custom, &full);
        assert_eq!(ids(&c), (Some("dev-desktop"), Some("dev-device")));
        assert_eq!(
            c.device.unwrap().client_secret.as_deref(),
            Some("dev-device-secret")
        );
        // Overrides win over a stale pairing too.
        let c = google_clients(GoogleProject::Release, &full);
        assert_eq!(ids(&c), (Some("dev-desktop"), Some("dev-device")));
    }

    #[test]
    fn partial_override_leaves_the_other_flow_unavailable() {
        let device_only = GoogleOAuthOverrides {
            device_client_id: Some("dev-device".into()),
            device_client_secret: Some("s".into()),
            ..Default::default()
        };
        let c = google_clients(GoogleProject::Custom, &device_only);
        assert_eq!(ids(&c), (None, Some("dev-device")));

        let loopback_only = GoogleOAuthOverrides {
            client_id: Some("dev-desktop".into()),
            client_secret: Some("s".into()),
            ..Default::default()
        };
        let c = google_clients(GoogleProject::Custom, &loopback_only);
        assert_eq!(ids(&c), (Some("dev-desktop"), None));

        // An id without its secret is no client (config validation refuses it).
        let half = GoogleOAuthOverrides {
            client_id: Some("dev-desktop".into()),
            ..Default::default()
        };
        assert_eq!(
            ids(&google_clients(GoogleProject::Custom, &half)),
            (None, None)
        );
        // `custom` with no overrides has no clients at all.
        let none = GoogleOAuthOverrides::default();
        assert_eq!(
            ids(&google_clients(GoogleProject::Custom, &none)),
            (None, None)
        );
    }

    #[test]
    fn agrees_with_config_google_flows() {
        use bunko_core::config::LibraryConfig;
        let mut l = LibraryConfig {
            google_project: GoogleProject::Custom,
            ..LibraryConfig::default()
        };
        l.oauth.google.device_client_id = Some("d".into());
        l.oauth.google.device_client_secret = Some("s".into());
        let c = google_clients(l.google_project, &l.oauth.google);
        assert_eq!(l.google_flows(), (c.loopback.is_some(), c.device.is_some()));
        for p in [GoogleProject::Release, GoogleProject::Beta] {
            let l = LibraryConfig {
                google_project: p,
                ..LibraryConfig::default()
            };
            let c = google_clients(l.google_project, &l.oauth.google);
            assert_eq!(l.google_flows(), (c.loopback.is_some(), c.device.is_some()));
        }
    }

    #[test]
    fn microsoft_default_and_override() {
        let c = microsoft_client(&MicrosoftOAuthOverrides::default());
        assert_eq!(c.client_id, MICROSOFT_CLIENT_ID);
        assert_eq!(c.client_secret, None);
        assert_eq!(c.provider, Provider::Microsoft);
        assert!(
            c.auth_url
                .starts_with(crate::auth::oauth::MICROSOFT_AUTHORITY)
        );
        assert_eq!(c.scopes, ["Files.ReadWrite", "offline_access", "User.Read"]);
        let c = microsoft_client(&MicrosoftOAuthOverrides {
            client_id: Some("other".into()),
        });
        assert_eq!(c.client_id, "other");
    }
}
