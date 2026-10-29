//! The reader's per-account tree under `/mokuro-reader`: which names belong to ONE
//! account instead of the shared library, and where they live.
//!
//! Every per-user table (the DAV tree, the metadata paths, the auth predicates, the
//! auth layer's physical mapping, `/login/api/me`) is derived from here, so a name
//! cannot be per-user in one place and shared in another.
//!
//! | rel under `/mokuro-reader` | what |
//! |---|---|
//! | `volume-data.json`, `profiles.json`, `goals.json`, `positions.json` | the account's root files |
//! | `history`, `history/<device>` | collections the reader creates (MKCOL) |
//! | `history/<device>/<file>` | the account's reading-history files |
//!
//! Names match exactly, case-sensitively: `Goals.json` and `History/` are library paths.

use std::path::{Path, PathBuf};

pub const READER_ROOT: &str = "mokuro-reader";

/// The account's root files, in the order `/login/api/me` has always reported them
/// (`per_user_files`; the reader ignores order). DAV listings use the same order.
pub const PER_USER_FILES: [&str; 4] = [
    "volume-data.json",
    "profiles.json",
    "goals.json",
    "positions.json",
];

/// The reading-history tree (0.8): one sub-folder per device, files inside it.
pub const HISTORY_DIR: &str = "history";
pub const PER_USER_DIRS: [&str; 1] = [HISTORY_DIR];

/// A history file is at most this big (413 beyond).
pub const HISTORY_MAX_FILE_BYTES: u64 = 16 << 20;
/// An account's whole history tree is at most this big (507 beyond).
pub const HISTORY_MAX_TOTAL_BYTES: u64 = 256 << 20;

/// A per-user reader-root-relative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerUser<'a> {
    /// One of [`PER_USER_FILES`].
    File(&'static str),
    /// One of [`PER_USER_DIRS`] itself.
    Dir(&'static str),
    /// Below one of [`PER_USER_DIRS`]; `rest` is relative to it (not empty).
    InDir { dir: &'static str, rest: &'a str },
}

/// Classify a reader-root-relative path (leading and trailing `/` ignored). Exact,
/// case-sensitive; `None` for a library path.
pub fn per_user(rel: &str) -> Option<PerUser<'_>> {
    let rel = rel.trim_matches('/');
    if let Some(name) = PER_USER_FILES.iter().find(|n| **n == rel) {
        return Some(PerUser::File(name));
    }
    for dir in PER_USER_DIRS {
        if rel == dir {
            return Some(PerUser::Dir(dir));
        }
        if let Some(rest) = rel.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) {
            let rest = rest.trim_start_matches('/');
            return Some(if rest.is_empty() {
                PerUser::Dir(dir)
            } else {
                PerUser::InDir { dir, rest }
            });
        }
    }
    None
}

pub fn is_per_user_rel(rel: &str) -> bool {
    per_user(rel).is_some()
}

/// Is `rel` the history tree or anything below it?
pub fn is_history_rel(rel: &str) -> bool {
    matches!(
        per_user(rel),
        Some(
            PerUser::Dir(HISTORY_DIR)
                | PerUser::InDir {
                    dir: HISTORY_DIR,
                    ..
                }
        )
    )
}

/// A name directly under the reader root that is the account's own (a file or a dir).
pub fn is_per_user_root_name(name: &str) -> bool {
    PER_USER_FILES.contains(&name) || PER_USER_DIRS.contains(&name)
}

/// A name that a case-insensitive comparison would take for a per-user root name
/// (`History`, `GOALS.json`): never merged with, or resolved into, the per-user tree.
pub fn folds_to_per_user_root_name(name: &str) -> bool {
    PER_USER_FILES
        .iter()
        .chain(PER_USER_DIRS.iter())
        .any(|n| n.eq_ignore_ascii_case(name))
}

/// What a path below `history/` may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryShape {
    /// `history` or `history/<seg>`: MKCOL.
    Collection,
    /// `history/<seg>/<file>`: PUT.
    File,
    Invalid,
}

/// The shape of the segments below `history/` (empty = `history` itself). Each segment
/// must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`, which also refuses `.`, `..` and
/// hidden names.
pub fn history_shape(rest_segments: &[&str]) -> HistoryShape {
    if !rest_segments.iter().all(|s| is_history_segment(s)) {
        return HistoryShape::Invalid;
    }
    match rest_segments.len() {
        0 | 1 => HistoryShape::Collection,
        2 => HistoryShape::File,
        _ => HistoryShape::Invalid,
    }
}

/// The shape of a reader-root-relative path, `None` when it is not under `history`.
pub fn history_shape_of_rel(rel: &str) -> Option<HistoryShape> {
    match per_user(rel)? {
        PerUser::Dir(HISTORY_DIR) => Some(HistoryShape::Collection),
        PerUser::InDir {
            dir: HISTORY_DIR,
            rest,
        } => {
            let segments: Vec<&str> = rest.split('/').collect();
            Some(history_shape(&segments))
        }
        _ => None,
    }
}

fn is_history_segment(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// The per-user names as `/login/api/me` advertises them: the files, then each
/// directory with a trailing `/` (a per-account directory tree).
pub fn advertised() -> Vec<String> {
    PER_USER_FILES
        .iter()
        .map(|f| f.to_string())
        .chain(PER_USER_DIRS.iter().map(|d| format!("{d}/")))
        .collect()
}

/// Who a per-user path is resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller<'a> {
    pub username: &'a str,
    /// This account owns the cloud root (a cloud library's progress user): its per-user
    /// files are the cloud root's own, shared with the cloud-direct reader.
    pub owns_cloud_root: bool,
}

/// A username that can be one directory name under `users/` (usernames are
/// `^[a-zA-Z0-9_-]{3,32}$` anyway).
pub fn safe_username(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// Where `caller`'s per-user tree lives: the library root for the cloud-root owner,
/// else `users/<username>`. `None` for a username that cannot be a directory.
pub fn user_base(library: &Path, users: &Path, caller: Caller<'_>) -> Option<PathBuf> {
    if caller.owns_cloud_root {
        return Some(library.to_path_buf());
    }
    safe_username(caller.username).then(|| users.join(caller.username))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_per_user_paths() {
        assert_eq!(per_user("goals.json"), Some(PerUser::File("goals.json")));
        assert_eq!(
            per_user("/positions.json/"),
            Some(PerUser::File("positions.json"))
        );
        assert_eq!(per_user("history"), Some(PerUser::Dir("history")));
        assert_eq!(per_user("history/"), Some(PerUser::Dir("history")));
        assert_eq!(
            per_user("history/dev/2026-10.events"),
            Some(PerUser::InDir {
                dir: "history",
                rest: "dev/2026-10.events"
            })
        );
        assert_eq!(per_user("History/x"), None);
        assert_eq!(per_user("Goals.json"), None);
        assert_eq!(per_user("historyx/y"), None);
        assert_eq!(per_user("S/goals.json"), None);
        assert_eq!(per_user(""), None);
        assert!(is_per_user_rel("history/a"));
        assert!(is_history_rel("history"));
        assert!(!is_history_rel("goals.json"));
        assert!(is_per_user_root_name("history"));
        assert!(is_per_user_root_name("volume-data.json"));
        assert!(!is_per_user_root_name("History"));
        assert!(folds_to_per_user_root_name("History"));
        assert!(folds_to_per_user_root_name("GOALS.JSON"));
        assert!(!folds_to_per_user_root_name("histories"));
    }

    #[test]
    fn advertised_keeps_the_me_order() {
        assert_eq!(
            advertised(),
            [
                "volume-data.json",
                "profiles.json",
                "goals.json",
                "positions.json",
                "history/"
            ]
        );
    }

    #[test]
    fn history_shapes() {
        use HistoryShape::*;
        assert_eq!(history_shape(&[]), Collection);
        assert_eq!(history_shape(&["dev-1"]), Collection);
        assert_eq!(history_shape(&["dev-1", "2026-10.events"]), File);
        assert_eq!(history_shape(&["a", "b", "c"]), Invalid);
        assert_eq!(history_shape(&[".hidden"]), Invalid);
        assert_eq!(history_shape(&["dev", ".."]), Invalid);
        assert_eq!(history_shape(&[".."]), Invalid);
        assert_eq!(history_shape(&["dev", ""]), Invalid);
        assert_eq!(history_shape(&["a b"]), Invalid);
        assert_eq!(history_shape(&[&"a".repeat(128)]), Collection);
        assert_eq!(history_shape(&[&"a".repeat(129)]), Invalid);
        assert_eq!(history_shape_of_rel("history"), Some(Collection));
        assert_eq!(history_shape_of_rel("history/d/x.events"), Some(File));
        assert_eq!(history_shape_of_rel("history/x.events"), Some(Collection));
        assert_eq!(history_shape_of_rel("history/a/b/c"), Some(Invalid));
        assert_eq!(history_shape_of_rel("goals.json"), None);
        assert_eq!(history_shape_of_rel("S/v.cbz"), None);
    }

    #[test]
    fn user_base_follows_the_cloud_root_owner() {
        let (lib, users) = (Path::new("/s/library"), Path::new("/s/users"));
        let me = Caller {
            username: "alice",
            owns_cloud_root: false,
        };
        assert_eq!(user_base(lib, users, me), Some(users.join("alice")));
        let owner = Caller {
            owns_cloud_root: true,
            ..me
        };
        assert_eq!(user_base(lib, users, owner), Some(lib.to_path_buf()));
        let bad = Caller {
            username: "..",
            owns_cloud_root: false,
        };
        assert_eq!(user_base(lib, users, bad), None);
    }
}
