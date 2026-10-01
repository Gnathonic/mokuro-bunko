//! Invites (spec §7) plus 0.5.2's `InviteManager` status helpers.
//!
//! Reproduced: 22-char `token_urlsafe(16)` codes never starting with `-`/`_`; only the
//! four invitable roles; `expires_at` stored as LOCAL naive `isoformat()` text (so a 0.5.2
//! rollback reads it); `validate_invite` valid at the exact instant; `list_invites`'s
//! non-`include_used` SQL kept verbatim with its lexicographic local-vs-UTC comparison
//! quirk (`T` > ` `); the `invite_created`/`invite_used` audit rows, with the plaintext
//! code as `target_path`.
//!
//! Choices:
//! - The invite insert and its `invite_created` audit row share one transaction, as do
//!   the `use_invite` update and `invite_used` (0.5.2 wrote them in two steps).
//! - An `expires_at` that does not parse (or carries a UTC offset) makes the invite
//!   invalid / "expired" instead of 0.5.2's unhandled exception (HTTP 500);
//!   `cleanup_expired_invites` skips such rows (0.5.2 skipped unparseable ones and
//!   crashed on offset ones).
//! - A duration too large for the calendar is refused with `Invalid duration format`
//!   (0.5.2: unhandled `OverflowError`).

use crate::audit::{AuditDetails, NewAuditEvent};
use crate::database::Database;
use crate::error::{DbError, Result};
use crate::pyfmt;
use crate::pytime::{fromisoformat, isoformat, local_now};
use crate::tokens::token_urlsafe;
use bunko_core::Role;
use chrono::{Duration, NaiveDateTime};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

/// The roles an invite may carry (never `admin` or `processor`), sorted as 0.5.2 prints
/// them.
pub const INVITABLE_ROLES: [Role; 4] = [
    Role::Editor,
    Role::Inviter,
    Role::Registered,
    Role::Uploader,
];
const INVITABLE_ROLES_TEXT: &str = "['editor', 'inviter', 'registered', 'uploader']";

/// 0.5.2 `InviteDict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invite {
    pub id: i64,
    pub code: String,
    pub role: Role,
    /// `YYYY-MM-DD HH:MM:SS` UTC.
    pub created_at: String,
    /// Local-naive ISO text (`2026-10-08T14:03:22.123456`), verbatim.
    pub expires_at: String,
    pub used_by: Option<String>,
    pub invited_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InviteStatus {
    Valid,
    Expired,
    Used,
}

/// 0.5.2 `InviteInfo` (the admin API's invite object; no `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InviteInfo {
    pub code: String,
    pub role: Role,
    pub status: InviteStatus,
    pub created_at: String,
    pub expires_at: String,
    pub used_by: Option<String>,
    pub invited_by: Option<String>,
}

impl Invite {
    /// The stored expiry as a local naive time; `None` if it does not parse or is zoned.
    pub fn expires_at_local(&self) -> Option<NaiveDateTime> {
        fromisoformat(&self.expires_at)
            .filter(|p| p.offset_seconds.is_none())
            .map(|p| p.naive)
    }

    /// `InviteManager.get_status`: used, else expired (now > expiry), else valid.
    pub fn status(&self) -> InviteStatus {
        if self.used_by.as_deref().is_some_and(|u| !u.is_empty()) {
            return InviteStatus::Used;
        }
        match self.expires_at_local() {
            Some(expiry) if local_now() <= expiry => InviteStatus::Valid,
            _ => InviteStatus::Expired,
        }
    }

    pub fn info(&self) -> InviteInfo {
        InviteInfo {
            code: self.code.clone(),
            role: self.role,
            status: self.status(),
            created_at: self.created_at.clone(),
            expires_at: self.expires_at.clone(),
            used_by: self.used_by.clone(),
            invited_by: self.invited_by.clone(),
        }
    }
}

struct RawInvite {
    id: i64,
    code: String,
    role: String,
    created_at: String,
    expires_at: String,
    used_by: Option<String>,
    invited_by: Option<String>,
}

impl RawInvite {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<RawInvite> {
        Ok(RawInvite {
            id: r.get(0)?,
            code: r.get(1)?,
            role: r.get(2)?,
            created_at: r.get(3)?,
            expires_at: r.get(4)?,
            used_by: r.get(5)?,
            invited_by: r.get(6)?,
        })
    }

    fn into_invite(self) -> Result<Invite> {
        let role = Role::from_str(&self.role).map_err(|_| DbError::CorruptRow {
            table: "invites",
            detail: format!("invite '{}' has unknown role '{}'", self.code, self.role),
        })?;
        Ok(Invite {
            id: self.id,
            code: self.code,
            role,
            created_at: self.created_at,
            expires_at: self.expires_at,
            used_by: self.used_by,
            invited_by: self.invited_by,
        })
    }
}

const INVITE_COLUMNS: &str = "id, code, role, created_at, expires_at, used_by, invited_by";

/// 0.5.2 `parse_duration`: `<int><h|d|w>` (unit case-insensitive), positive.
pub fn parse_duration(duration: &str) -> Result<Duration> {
    if duration.is_empty() {
        return Err(DbError::invalid("Duration cannot be empty"));
    }
    let mut chars = duration.chars();
    let last = chars.next_back().unwrap_or_default();
    let number = chars.as_str();
    let invalid = || DbError::Invalid(format!("Invalid duration format: {duration}"));
    let value = parse_py_int(number).ok_or_else(invalid)?;
    if value <= 0 {
        return Err(DbError::Invalid(format!(
            "Duration must be positive: {duration}"
        )));
    }
    let unit: String = last.to_lowercase().collect();
    let out = match unit.as_str() {
        "h" => Duration::try_hours(value),
        "d" => Duration::try_days(value),
        "w" => Duration::try_weeks(value),
        _ => return Err(DbError::Invalid(format!("Unknown duration unit: {unit}"))),
    };
    out.ok_or_else(invalid)
}

/// Python `int(text)` for ASCII digits: surrounding whitespace, an optional sign, and
/// single underscores between digits.
fn parse_py_int(text: &str) -> Option<i64> {
    let t = pyfmt::strip(text);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let v: i64 = digits.replace('_', "").parse().ok()?;
    Some(if negative { -v } else { v })
}

impl Database {
    /// Create an invite; returns its code. Logs `invite_created` (actor = `invited_by`).
    pub fn create_invite(
        &self,
        role: Role,
        expires: &str,
        invited_by: Option<&str>,
    ) -> Result<String> {
        if !INVITABLE_ROLES.contains(&role) {
            return Err(DbError::Invalid(format!(
                "Role cannot be granted by invite: {role}. Must be one of: {INVITABLE_ROLES_TEXT}"
            )));
        }
        let mut code = token_urlsafe(16);
        // A leading '-' is misread as an option by CLI parsing: regenerate.
        while code.starts_with(['-', '_']) {
            code = token_urlsafe(16);
        }
        let duration = parse_duration(expires)?;
        let expires_at = local_now()
            .checked_add_signed(duration)
            .ok_or_else(|| DbError::Invalid(format!("Invalid duration format: {expires}")))?;
        let expires_text = isoformat(&expires_at);
        self.write(|conn| {
            conn.execute(
                "INSERT INTO invites (code, role, invited_by, expires_at) VALUES (?, ?, ?, ?)",
                params![code, role.as_str(), invited_by, expires_text],
            )?;
            let event = NewAuditEvent::new("invite_created")
                .actor(invited_by)
                .target_type("invite")
                .target_path(&code)
                .details(
                    AuditDetails::new()
                        .with("role", role.as_str())
                        .with("expires", expires),
                );
            self.log_audit_in(conn, &event)?;
            Ok(())
        })?;
        Ok(code)
    }

    /// The invite with this code, whatever its state.
    pub fn get_invite(&self, code: &str) -> Result<Option<Invite>> {
        let raw = self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {INVITE_COLUMNS} FROM invites WHERE code = ?"
                ))?
                .query_row([code], RawInvite::from_row)
                .optional()?)
        })?;
        raw.map(RawInvite::into_invite).transpose()
    }

    /// The invite if it exists, is unused and not expired (valid at the exact instant).
    pub fn validate_invite(&self, code: &str) -> Result<Option<Invite>> {
        let invite = match self.get_invite(code) {
            Ok(Some(invite)) => invite,
            Ok(None) => return Ok(None),
            Err(DbError::CorruptRow { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok((invite.status() == InviteStatus::Valid).then_some(invite))
    }

    /// Consume a valid invite for `username` (logs `invite_used`). `false` if the code is
    /// not valid or a concurrent consumer won.
    pub fn use_invite(&self, code: &str, username: &str) -> Result<bool> {
        let Some(invite) = self.validate_invite(code)? else {
            return Ok(false);
        };
        self.write(|conn| {
            let changed = conn.execute(
                "UPDATE invites SET used_by = ?, used_at = datetime('now') \
                 WHERE code = ? AND used_by IS NULL",
                params![username, code],
            )? > 0;
            if changed {
                let invited_by = invite.invited_by.clone().map_or(Value::Null, Value::String);
                let event = NewAuditEvent::new("invite_used")
                    .actor(Some(username))
                    .target_type("invite")
                    .target_path(code)
                    .target_username(username)
                    .details(
                        AuditDetails::new()
                            .with("invited_by", invited_by)
                            .with("role", invite.role.as_str()),
                    );
                self.log_audit_in(conn, &event)?;
            }
            Ok(changed)
        })
    }

    /// Invites, newest first: all of them, or (0.5.2's SQL, quirk included) the unused
    /// ones whose `expires_at` text sorts after `datetime('now')`.
    pub fn list_invites(&self, include_used: bool) -> Result<Vec<Invite>> {
        let sql = if include_used {
            format!("SELECT {INVITE_COLUMNS} FROM invites ORDER BY created_at DESC")
        } else {
            format!(
                "SELECT {INVITE_COLUMNS} FROM invites \
                 WHERE used_by IS NULL AND expires_at > datetime('now') \
                 ORDER BY created_at DESC"
            )
        };
        let raws = self.read(|conn| {
            Ok(conn
                .prepare_cached(&sql)?
                .query_map([], RawInvite::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        Ok(raws
            .into_iter()
            .filter_map(|raw| match raw.into_invite() {
                Ok(invite) => Some(invite),
                Err(e) => {
                    tracing::warn!(error = %e, "list_invites: skipping an unreadable row");
                    None
                }
            })
            .collect())
    }

    /// `InviteManager.get_info`.
    pub fn invite_info(&self, code: &str) -> Result<Option<InviteInfo>> {
        Ok(self.get_invite(code)?.map(|i| i.info()))
    }

    /// `InviteManager.list_all`: every invite with its status (the admin API's list).
    pub fn list_invite_infos(&self) -> Result<Vec<InviteInfo>> {
        Ok(self.list_invites(true)?.iter().map(Invite::info).collect())
    }

    /// Delete an invite (used and expired ones too). `false` if there was none.
    pub fn delete_invite(&self, code: &str) -> Result<bool> {
        self.write(|conn| Ok(conn.execute("DELETE FROM invites WHERE code = ?", [code])? > 0))
    }

    /// Delete unused invites whose (local-naive) expiry has passed; how many. Never
    /// scheduled by 0.5.2; kept as a library function.
    pub fn cleanup_expired_invites(&self) -> Result<usize> {
        let now = local_now();
        self.write(|conn| {
            let rows: Vec<(i64, Option<String>)> = conn
                .prepare("SELECT id, expires_at FROM invites WHERE used_by IS NULL")?
                .query_map([], |r| Ok((r.get(0)?, crate::series::opt_text(r, 1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let expired: Vec<i64> = rows
                .into_iter()
                .filter(|(_, at)| {
                    at.as_deref()
                        .and_then(fromisoformat)
                        .filter(|p| p.offset_seconds.is_none())
                        .is_some_and(|p| p.naive < now)
                })
                .map(|(id, _)| id)
                .collect();
            if expired.is_empty() {
                return Ok(0);
            }
            let sql = format!(
                "DELETE FROM invites WHERE id IN ({})",
                vec!["?"; expired.len()].join(",")
            );
            Ok(conn.execute(&sql, rusqlite::params_from_iter(expired.iter()))?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;
    use chrono::Duration as D;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h").unwrap(), D::hours(1));
        assert_eq!(parse_duration("7d").unwrap(), D::days(7));
        assert_eq!(parse_duration("2W").unwrap(), D::weeks(2));
        assert_eq!(parse_duration(" 7d").unwrap(), D::days(7));
        assert_eq!(parse_duration("1_0d").unwrap(), D::days(10));
        let e = |s: &str| parse_duration(s).unwrap_err().to_string();
        assert_eq!(e(""), "Duration cannot be empty");
        assert_eq!(e("d"), "Invalid duration format: d");
        assert_eq!(e("xd"), "Invalid duration format: xd");
        assert_eq!(e("7x"), "Unknown duration unit: x");
        assert_eq!(e("-1d"), "Duration must be positive: -1d");
        assert_eq!(e("0h"), "Duration must be positive: 0h");
        assert_eq!(
            e("99999999999999w"),
            "Invalid duration format: 99999999999999w"
        );
    }

    #[test]
    fn create_validate_use() {
        let (_dir, db) = temp_db();
        let code = db
            .create_invite(Role::Uploader, "7d", Some("root"))
            .unwrap();
        assert_eq!(code.len(), 22);
        assert!(!code.starts_with(['-', '_']));
        let inv = db.get_invite(&code).unwrap().unwrap();
        assert_eq!(inv.role, Role::Uploader);
        assert_eq!(inv.invited_by.as_deref(), Some("root"));
        assert!(inv.expires_at.contains('T'));
        assert_eq!(inv.status(), InviteStatus::Valid);
        assert!(db.validate_invite(&code).unwrap().is_some());
        assert!(db.validate_invite("nope").unwrap().is_none());
        assert!(db.use_invite(&code, "newbie").unwrap());
        assert!(!db.use_invite(&code, "other").unwrap(), "only once");
        assert!(db.validate_invite(&code).unwrap().is_none());
        assert_eq!(
            db.invite_info(&code).unwrap().unwrap().status,
            InviteStatus::Used
        );
        let events = db.list_audit_events(10, None).unwrap();
        assert_eq!(events[0].action, "invite_used");
        assert_eq!(
            events[0].details.as_deref(),
            Some(r#"{"invited_by":"root","role":"uploader"}"#)
        );
        assert_eq!(events[0].target_username.as_deref(), Some("newbie"));
        assert_eq!(events[1].action, "invite_created");
        assert_eq!(events[1].target_path.as_deref(), Some(code.as_str()));
        assert_eq!(
            events[1].details.as_deref(),
            Some(r#"{"role":"uploader","expires":"7d"}"#)
        );
        assert_eq!(events[1].actor_username.as_deref(), Some("root"));
    }

    #[test]
    fn only_invitable_roles() {
        let (_dir, db) = temp_db();
        for role in [Role::Admin, Role::Processor, Role::Anonymous] {
            assert_eq!(
                db.create_invite(role, "7d", None).unwrap_err().to_string(),
                format!(
                    "Role cannot be granted by invite: {role}. Must be one of: ['editor', 'inviter', 'registered', 'uploader']"
                )
            );
        }
        assert!(db.create_invite(Role::Uploader, "nope", None).is_err());
        assert!(
            db.list_invites(true).unwrap().is_empty(),
            "a bad duration creates nothing"
        );
    }

    #[test]
    fn listing_deleting_and_cleanup() {
        let (_dir, db) = temp_db();
        let live = db.create_invite(Role::Registered, "7d", None).unwrap();
        let used = db.create_invite(Role::Registered, "7d", None).unwrap();
        db.use_invite(&used, "someone").unwrap();
        let past = isoformat(&(local_now() - D::days(2)));
        db.with_writer_connection(|c| {
            c.execute("INSERT INTO invites (code, role, expires_at) VALUES ('old1', 'registered', ?)", [&past])
                .unwrap();
            c.execute("INSERT INTO invites (code, role, expires_at) VALUES ('bad1', 'registered', 'not a date')", [])
                .unwrap();
        });
        let valid: Vec<_> = db
            .list_invites(false)
            .unwrap()
            .into_iter()
            .map(|i| i.code)
            .collect();
        assert!(
            valid.contains(&live) && !valid.contains(&used) && !valid.contains(&"old1".to_string())
        );
        assert_eq!(db.list_invites(true).unwrap().len(), 4);
        let infos = db.list_invite_infos().unwrap();
        let status = |c: &str| infos.iter().find(|i| i.code == c).unwrap().status;
        assert_eq!(status(&live), InviteStatus::Valid);
        assert_eq!(status(&used), InviteStatus::Used);
        assert_eq!(status("old1"), InviteStatus::Expired);
        assert_eq!(status("bad1"), InviteStatus::Expired);
        assert!(db.validate_invite("bad1").unwrap().is_none());
        assert_eq!(
            db.cleanup_expired_invites().unwrap(),
            1,
            "only old1; the bad row is skipped"
        );
        assert!(db.delete_invite(&used).unwrap());
        assert!(!db.delete_invite(&used).unwrap());
        let json = serde_json::to_value(db.invite_info(&live).unwrap().unwrap()).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        let mut want = vec![
            "code",
            "role",
            "status",
            "created_at",
            "expires_at",
            "used_by",
            "invited_by",
        ];
        want.sort();
        let mut got = keys.clone();
        got.sort();
        assert_eq!(got, want);
    }
}
