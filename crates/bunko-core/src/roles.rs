//! User roles. A strict hierarchy, plus the separate `processor` role for OCR machines.

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
