//! Physical correction files nobody owns (D27). 0.8 resolves `<Vol>.edit[-n].mokuro`
//! per account, so such a file written into the shared folder before 0.8 (a hand-made
//! layer slugged `edit`, an early reader push) is invisible to everyone.
//!
//! On a library on this disk each one goes to the account that last wrote it (the
//! audit log), into that account's overlay. One with no active writer on record stays
//! where it is, hidden, and is counted in the storage status. A cloud library's are
//! its progress user's; with none chosen they are only counted.

use bunko_dav::PersonalOverlays;
use bunko_db::{Database, NewAuditEvent, UserStatus};

/// What one sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CorrectionSweep {
    /// Moved into their writer's overlay.
    pub adopted: usize,
    /// Left in place: nobody sees them.
    pub unowned: usize,
}

/// Give each physical correction to its last writer (`adopt`), or only count them.
/// Blocking. Idempotent: an adopted file is gone from the library.
pub fn adopt_unowned_corrections(
    db: &Database,
    overlays: &PersonalOverlays,
    adopt: bool,
) -> CorrectionSweep {
    let mut out = CorrectionSweep::default();
    for rel in overlays.physical_corrections() {
        let target = format!("/{}/{rel}", bunko_core::reader_tree::READER_ROOT);
        let writer = adopt.then(|| active_writer(db, &target)).flatten();
        match writer {
            Some(user) if overlays.adopt(&rel, &user) => {
                tracing::info!("[DAV] the correction {rel} is now {user}'s own");
                let event = NewAuditEvent::new("correction_adopted")
                    .target_type("personal")
                    .target_path(&target)
                    .target_username(&user);
                if let Err(e) = db.log_audit_event(&event) {
                    tracing::warn!("audit of an adopted correction failed: {e}");
                }
                out.adopted += 1;
            }
            _ => out.unowned += 1,
        }
    }
    if out.unowned > 0 {
        tracing::warn!(
            "[DAV] {} correction file(s) in the library have no owner: nobody sees them",
            out.unowned
        );
    }
    out
}

/// The last writer of the shared file `target`, while that account is active.
fn active_writer(db: &Database, target: &str) -> Option<String> {
    let name = db.last_library_writer(target).ok()??;
    let user = db.get_user(&name).ok()??;
    (user.status == UserStatus::Active).then_some(user.username)
}
