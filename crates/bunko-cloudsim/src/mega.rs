//! MEGA, mounted under `{base}/mega/` (the gateway, `ClientConfig.api_origin` or
//! megajs's `gateway`). The mock itself stays in `bunko-mega` (feature `mock`): it needs
//! that crate's crypto internals to encrypt nodes for real. Accounts log in by password
//! (created with [`crate::CloudSim::mega_account`] or `POST /_sim/mega/account`) or
//! resume a session blob.

use serde_json::Value;

pub use bunko_mega::mock::{INBOX, MegaHub, ROOT, RUBBISH};

/// One MEGA account: the `bunko_mega` mock's knob and helper surface.
pub type MegaHandle = bunko_mega::mock::MockMega;

/// The path prefix of the gateway.
pub const PREFIX: &str = "/mega";

fn as_bool(v: &Value, k: &str) -> Result<bool, String> {
    v.as_bool()
        .ok_or_else(|| format!("knob {k} takes true or false"))
}

fn as_u64(v: &Value, k: &str) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("knob {k} takes a non-negative number"))
}

/// Set one knob on a MEGA account (`POST /_sim/knobs`).
pub fn knob(m: &MegaHandle, k: &str, v: &Value) -> Result<(), String> {
    match k {
        "eagain" => m.eagain(as_u64(v, k)? as usize),
        "overquota" => m.overquota(as_bool(v, k)?),
        "bandwidth_509" => m.bandwidth_509(as_bool(v, k)?),
        "garbage_download" => m.garbage_download(as_bool(v, k)?),
        "require_hashcash" => {
            let e = as_u64(v, k)?;
            m.require_hashcash(u8::try_from(e).map_err(|_| "require_hashcash is 0-255")?);
        }
        "drop_upload_at" => m.drop_upload_at(as_u64(v, k)?),
        "lose_cursor" => {
            if as_bool(v, k)? {
                m.lose_cursor();
            }
        }
        "kill_session" => {
            if as_bool(v, k)? {
                m.kill_session();
            }
        }
        "fail_command" => {
            let cmd = v["command"]
                .as_str()
                .ok_or("fail_command takes {command, code}")?;
            let code = v["code"]
                .as_i64()
                .ok_or("fail_command takes {command, code}")?;
            m.fail_command(cmd, code);
        }
        _ => return Err(format!("no MEGA knob {k:?}")),
    }
    Ok(())
}
