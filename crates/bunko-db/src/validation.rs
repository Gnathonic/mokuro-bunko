//! Username/password validation (0.5.2 `validation.py`) and bcrypt hashing.
//!
//! Choices (spec §4):
//! - The username pattern is anchored at the true end of the string. Python's `$` also
//!   matched before a trailing `\n`, so `"abc\n"` passed there; it is refused here (fix —
//!   only reachable through the CLI).
//! - Password length counts Unicode code points, as Python `len` does.
//! - bcrypt: cost 12, `$2b$`, the UTF-8 bytes of the password. Like the `bcrypt` 4.3 wheel
//!   in the 0.5.2 environment (itself built on this same Rust crate), only the first 72
//!   bytes count and NUL bytes are ordinary bytes; the golden tests pin both against
//!   Python-made hashes. A malformed stored hash verifies as "wrong password" instead of
//!   0.5.2's unhandled `ValueError`.

pub const MIN_PASSWORD_LENGTH: usize = 8;
pub const MAX_PASSWORD_LENGTH: usize = 128;
/// bcrypt cost 0.5.2 uses (`bcrypt.gensalt()` default).
pub const BCRYPT_COST: u32 = 12;

pub const USERNAME_REQUIRED: &str = "Username is required";
pub const USERNAME_FORMAT: &str =
    "Username must be 3-32 characters and contain only letters, numbers, underscores, and hyphens";

/// `validate_username`: the error message, or `None` when acceptable.
pub fn validate_username(username: &str) -> Option<&'static str> {
    if username.is_empty() {
        return Some(USERNAME_REQUIRED);
    }
    let ok_len = (3..=32).contains(&username.len());
    let ok_chars = username
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok_len && ok_chars {
        None
    } else {
        Some(USERNAME_FORMAT)
    }
}

/// `validate_password`: the error message, or `None` when acceptable.
pub fn validate_password(password: &str) -> Option<&'static str> {
    if password.is_empty() {
        return Some("Password is required");
    }
    let len = password.chars().count();
    if len < MIN_PASSWORD_LENGTH {
        return Some("Password must be at least 8 characters");
    }
    if len > MAX_PASSWORD_LENGTH {
        return Some("Password must be at most 128 characters");
    }
    None
}

/// `bcrypt.hashpw(password.encode(), bcrypt.gensalt(cost))`.
pub fn hash_password(password: &str, cost: u32) -> Result<String, bcrypt::BcryptError> {
    bcrypt::hash(password.as_bytes(), cost)
}

/// `bcrypt.checkpw(password.encode(), hash.encode())`; a malformed hash is `false`.
pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password.as_bytes(), hash).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert_eq!(validate_username(""), Some(USERNAME_REQUIRED));
        assert_eq!(validate_username("ab"), Some(USERNAME_FORMAT));
        assert_eq!(validate_username("abc"), None);
        assert_eq!(validate_username("a-b_c9"), None);
        assert_eq!(validate_username(&"a".repeat(32)), None);
        assert_eq!(validate_username(&"a".repeat(33)), Some(USERNAME_FORMAT));
        assert_eq!(validate_username("abc\n"), Some(USERNAME_FORMAT));
        assert_eq!(validate_username("ab c"), Some(USERNAME_FORMAT));
        assert_eq!(validate_username("\u{e9}bc"), Some(USERNAME_FORMAT));
    }

    #[test]
    fn passwords_count_code_points() {
        assert_eq!(validate_password(""), Some("Password is required"));
        assert_eq!(
            validate_password("1234567"),
            Some("Password must be at least 8 characters")
        );
        assert_eq!(validate_password("12345678"), None);
        // 8 code points, 16 bytes.
        assert_eq!(validate_password(&"\u{e9}".repeat(8)), None);
        assert_eq!(validate_password(&"\u{e9}".repeat(128)), None);
        assert_eq!(
            validate_password(&"x".repeat(129)),
            Some("Password must be at most 128 characters")
        );
    }

    #[test]
    fn bcrypt_shape_and_truncation() {
        let h = hash_password("password123", 4).unwrap();
        assert!(h.starts_with("$2b$04$") && h.len() == 60);
        assert!(verify_password("password123", &h));
        assert!(!verify_password("password124", &h));
        let long = "\u{e9}".repeat(50); // 100 bytes
        let h = hash_password(&long, 4).unwrap();
        assert!(verify_password(&"\u{e9}".repeat(36), &h)); // the first 72 bytes
        assert!(!verify_password(&"\u{e9}".repeat(35), &h));
        assert!(!verify_password("x", "garbage"));
    }
}
