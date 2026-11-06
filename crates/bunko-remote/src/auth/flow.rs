//! Flow selection (§7): the user sees one "Connect" button.

use super::oauth::Provider;

/// The sign-in flow a Connect click runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Loopback redirect + PKCE: the browser on this machine comes back to Bunko.
    Loopback,
    /// Device code: the user types a code on any device (Docker, NAS, a LAN browser,
    /// the Android WebView, the CLI). Microsoft refuses it for personal accounts
    /// (AADSTS70002), so it is OneDrive's choice only on request (work or school).
    Device,
    /// The loopback redirect without a listener: the user signs in on any device and
    /// pastes back the address their browser ends on. OneDrive's flow wherever the
    /// browser cannot come back to Bunko.
    Paste,
}

impl Flow {
    /// The name the API and the pages use (`loopback`, `device`, `paste`).
    pub fn as_str(self) -> &'static str {
        match self {
            Flow::Loopback => "loopback",
            Flow::Device => "device",
            Flow::Paste => "paste",
        }
    }

    /// A flow by its API name (`browser` is the CLI's word for `loopback`).
    pub fn parse(s: &str) -> Option<Flow> {
        match s {
            "loopback" | "browser" => Some(Flow::Loopback),
            "device" | "code" => Some(Flow::Device),
            "paste" => Some(Flow::Paste),
            _ => None,
        }
    }
}

/// The flow nobody asked for by name: loopback when the wizard or admin request comes
/// from loopback and there is a loopback client; otherwise the device flow for Google
/// and the pasted address for Microsoft (whose device flow fails personal accounts).
pub fn choose_flow(provider: Provider, request_is_loopback: bool, has_loopback: bool) -> Flow {
    if request_is_loopback && has_loopback {
        Flow::Loopback
    } else if provider == Provider::Microsoft {
        Flow::Paste
    } else {
        Flow::Device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_needs_both() {
        for p in [Provider::Google, Provider::Microsoft] {
            assert_eq!(choose_flow(p, true, true), Flow::Loopback);
        }
        assert_eq!(choose_flow(Provider::Google, true, false), Flow::Device);
        assert_eq!(choose_flow(Provider::Google, false, true), Flow::Device);
        assert_eq!(choose_flow(Provider::Google, false, false), Flow::Device);
    }

    #[test]
    fn microsoft_elsewhere_pastes() {
        assert_eq!(choose_flow(Provider::Microsoft, false, true), Flow::Paste);
        assert_eq!(choose_flow(Provider::Microsoft, true, false), Flow::Paste);
    }

    #[test]
    fn names_round_trip() {
        for f in [Flow::Loopback, Flow::Device, Flow::Paste] {
            assert_eq!(Flow::parse(f.as_str()), Some(f));
        }
        assert_eq!(Flow::parse("browser"), Some(Flow::Loopback));
        assert_eq!(Flow::parse("magic"), None);
    }
}
