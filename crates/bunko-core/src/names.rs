//! Which file names are one entry: the one filename fold.
//!
//! Bunko works to the lowest common denominator of the filesystems a library lives on,
//! so it joins every pair of names any of them joins: Linux casefold (full Unicode case
//! folding, `Straße` = `STRASSE`, `ſ` = `s`, `ς` = `σ`), APFS (canonical
//! normalization: a decomposed `é` = the composed one), NTFS and exFAT (a simple
//! uppercase table: `ı` = `i`). Two names are one entry when [`fold_name`] gives the
//! same string for both, on every host, a remote store included.

use unicode_normalization::UnicodeNormalization;

use crate::casefold_table::CASEFOLD;

/// The full case folding of `c` (CaseFolding.txt status C and F), appended to `out`.
fn push_casefold(out: &mut String, c: char) {
    match CASEFOLD.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
        Ok(i) => out.push_str(CASEFOLD[i].1),
        Err(_) => out.push(c),
    }
}

fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_casefold(&mut out, c);
    }
    out
}

/// The form two names that are one entry share: canonical decomposition, full Unicode
/// case folding, then the case folding of its uppercase (so a letter an uppercase table
/// joins with another, `ı` with `i`, folds alike too), recomposed (NFC). Idempotent.
/// ASCII names fold to their ASCII lowercase.
pub fn fold_name(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    let decomposed: String = name.nfd().collect();
    let folded = casefold(&decomposed);
    let again = casefold(&folded.to_uppercase());
    again.nfd().collect::<String>().nfc().collect()
}

#[cfg(test)]
mod tests {
    use super::fold_name;

    #[test]
    fn names_any_host_joins_fold_alike() {
        let same = [
            ("Kingdom", "KINGDOM"),
            ("Straße", "STRASSE"),
            ("Straße", "strasse"),
            ("ẞ", "ss"),
            ("ſ.cbz", "s.cbz"),
            ("ΟΔΟΣ", "οδος"),
            ("οδος", "οδοσ"),
            ("\u{212A}", "k"),
            ("ı", "I"),
            ("ﬁle", "FILE"),
            ("Pok\u{e9}mon", "POKE\u{301}MON"),
            ("\u{1F1}", "\u{1F3}"),
            ("\u{130}", "i\u{307}"),
        ];
        for (a, b) in same {
            assert_eq!(fold_name(a), fold_name(b), "{a:?} and {b:?}");
        }
        assert_ne!(fold_name("a"), fold_name("b"));
        assert_ne!(fold_name("Vol 1"), fold_name("Vol 2"));
        assert_eq!(fold_name("Kingdom/Vol 1.CBZ"), "kingdom/vol 1.cbz");
    }

    #[test]
    fn the_fold_is_idempotent() {
        for name in [
            "Straße",
            "ΟΔΟΣ ς",
            "\u{130}stanbul",
            "ǅemal",
            "POKE\u{301}MON",
            "ﬃ",
            "\u{345}\u{3b1}",
            "J̌",
        ] {
            let once = fold_name(name);
            assert_eq!(fold_name(&once), once, "{name:?}");
        }
    }
}
