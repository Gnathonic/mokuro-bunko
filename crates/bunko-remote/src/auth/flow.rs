//! Flow selection (§7): the user sees one "Connect" button.

/// The sign-in flow a Connect click runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Loopback redirect + PKCE: the browser on this machine comes back to Bunko.
    Loopback,
    /// Device code: the user types a code on any device (Docker, NAS, a LAN browser,
    /// the Android WebView, the CLI).
    Device,
}

/// Loopback only when the wizard or admin request itself comes from loopback and
/// the host can open a browser; the device flow otherwise.
pub fn choose_flow(request_is_loopback: bool, can_open_browser: bool) -> Flow {
    if request_is_loopback && can_open_browser {
        Flow::Loopback
    } else {
        Flow::Device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_needs_both() {
        assert_eq!(choose_flow(true, true), Flow::Loopback);
        assert_eq!(choose_flow(true, false), Flow::Device);
        assert_eq!(choose_flow(false, true), Flow::Device);
        assert_eq!(choose_flow(false, false), Flow::Device);
    }
}
