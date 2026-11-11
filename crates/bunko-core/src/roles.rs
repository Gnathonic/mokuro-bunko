//! User roles and the capabilities they grant. A strict hierarchy, plus the separate
//! `processor` role for OCR machines.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Anonymous,
    Registered,
    Uploader,
    Editor,
    Inviter,
    Admin,
    /// A remote OCR machine: may read the library and run OCR for it, nothing else.
    Processor,
}

impl Role {
    pub const ALL: [Role; 7] = [
        Role::Anonymous,
        Role::Registered,
        Role::Uploader,
        Role::Editor,
        Role::Inviter,
        Role::Admin,
        Role::Processor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Anonymous => "anonymous",
            Role::Registered => "registered",
            Role::Uploader => "uploader",
            Role::Editor => "editor",
            Role::Inviter => "inviter",
            Role::Admin => "admin",
            Role::Processor => "processor",
        }
    }

    /// Rank in the human hierarchy (Admin > Inviter > Editor > Uploader > Registered >
    /// Anonymous). `processor` is outside it and ranks as `registered` for reads only;
    /// callers that gate writes must check for it explicitly.
    pub fn level(self) -> u8 {
        match self {
            Role::Anonymous => 0,
            Role::Registered | Role::Processor => 1,
            Role::Uploader => 2,
            Role::Editor => 3,
            Role::Inviter => 4,
            Role::Admin => 5,
        }
    }

    /// True when this role includes every capability of `other` (human hierarchy only).
    pub fn at_least(self, other: Role) -> bool {
        match (self, other) {
            (Role::Processor, Role::Processor | Role::Anonymous | Role::Registered) => true,
            (Role::Processor, _) | (_, Role::Processor) => false,
            _ => self.level() >= other.level(),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "anonymous" => Role::Anonymous,
            "registered" => Role::Registered,
            // Legacy name, migrated by 0.3.
            "uploader" | "writer" => Role::Uploader,
            "editor" => Role::Editor,
            "inviter" => Role::Inviter,
            "admin" => Role::Admin,
            "processor" => Role::Processor,
            other => return Err(format!("unknown role '{other}'")),
        })
    }
}

/// Capabilities derived from a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    Read,
    WriteProgress,
    AddFiles,
    ModifyDelete,
    ManageInvites,
    Admin,
    Process,
}

pub fn has_perm(role: Role, perm: Perm) -> bool {
    use Role::*;
    match perm {
        Perm::Read => true,
        Perm::WriteProgress => matches!(role, Registered | Uploader | Inviter | Editor | Admin),
        Perm::AddFiles => matches!(role, Uploader | Inviter | Editor | Admin),
        Perm::ModifyDelete => matches!(role, Inviter | Editor | Admin),
        Perm::ManageInvites => matches!(role, Inviter | Admin),
        Perm::Admin => role == Admin,
        Perm::Process => role == Processor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability table as 0.7 shipped it (bunko-server `auth`), row per role in
    /// `Role::ALL` order, columns Read, WriteProgress, AddFiles, ModifyDelete,
    /// ManageInvites, Admin, Process.
    #[test]
    fn has_perm_keeps_the_07_table() {
        let perms = [
            Perm::Read,
            Perm::WriteProgress,
            Perm::AddFiles,
            Perm::ModifyDelete,
            Perm::ManageInvites,
            Perm::Admin,
            Perm::Process,
        ];
        #[rustfmt::skip]
        let table: [(Role, [bool; 7]); 7] = [
            (Role::Anonymous, [true, false, false, false, false, false, false]),
            (Role::Registered, [true, true, false, false, false, false, false]),
            (Role::Uploader, [true, true, true, false, false, false, false]),
            (Role::Editor, [true, true, true, true, false, false, false]),
            (Role::Inviter, [true, true, true, true, true, false, false]),
            (Role::Admin, [true, true, true, true, true, true, false]),
            (Role::Processor, [true, false, false, false, false, false, true]),
        ];
        for (role, row) in table {
            for (perm, want) in perms.iter().zip(row) {
                assert_eq!(has_perm(role, *perm), want, "{role} {perm:?}");
            }
        }
    }
}
