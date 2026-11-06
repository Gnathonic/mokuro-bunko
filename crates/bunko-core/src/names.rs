//! Which file names are one entry: the one filename fold.
//!
//! Bunko works to the lowest common denominator of the filesystems a library lives on,
//! so it joins every pair of names any of them joins: Linux casefold (full Unicode case
//! folding, `Straße` = `STRASSE`, `ſ` = `s`, `ς` = `σ`), APFS (canonical
//! normalization: a decomposed `é` = the composed one), NTFS and exFAT (a simple
//! uppercase table: `ı` = `i`). Linux casefold also drops every Unicode
//! default-ignorable code point when it compares names (zero width space and joiners,
//! bidi marks, variation selectors, the soft hyphen, the Hangul fillers, tag
//! characters: `Vol\u{200B}1` = `Vol1`), so the fold drops them too. Two names are one
//! entry when [`fold_name`] gives the same string for both, on every host, a remote
//! store included.
//!
//! This is the FILENAME fold only. Sidecar names rule v1 (which files are a volume's,
//! the contract with Mokuro Reader) folds stems its own way
//! ([`crate::sidecar_names::stem_fold`]) and is not changed by it.

use unicode_normalization::UnicodeNormalization;

use crate::casefold_table::CASEFOLD;
use crate::ignorable_table::DEFAULT_IGNORABLE;

/// Is `c` a Default_Ignorable_Code_Point (UCD 15.0.0, [`DEFAULT_IGNORABLE`])?
pub fn is_default_ignorable(c: char) -> bool {
    let cp = c as u32;
    DEFAULT_IGNORABLE
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

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

/// The form two names that are one entry share: the default-ignorable code points
/// dropped, canonical decomposition, full Unicode case folding, then the case folding
/// of its uppercase (so a letter an uppercase table joins with another, `ı` with `i`,
/// folds alike too), recomposed (NFC). Idempotent. ASCII names fold to their ASCII
/// lowercase.
pub fn fold_name(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    let decomposed: String = name
        .chars()
        .filter(|c| !is_default_ignorable(*c))
        .nfd()
        .collect();
    let folded = casefold(&decomposed);
    let again = casefold(&folded.to_uppercase());
    again
        .nfd()
        .collect::<String>()
        .nfc()
        .filter(|c| !is_default_ignorable(*c))
        .collect()
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

    /// Every Default_Ignorable_Code_Point (the generated UCD 15.0.0 table) is dropped,
    /// wherever it stands in a name, as Linux casefold drops it: a name that differs
    /// from another only by such code points is that name. And only those are: the
    /// characters around each range still count.
    #[test]
    fn default_ignorable_code_points_are_no_part_of_a_name() {
        use crate::ignorable_table::DEFAULT_IGNORABLE;
        let mut count = 0;
        for &(lo, hi) in DEFAULT_IGNORABLE {
            for cp in lo..=hi {
                let c = char::from_u32(cp).expect("no surrogate is default-ignorable");
                count += 1;
                assert!(super::is_default_ignorable(c), "U+{cp:04X}");
                for name in [
                    format!("Vol{c}1.cbz"),
                    format!("{c}Vol1.cbz"),
                    format!("Vol1.cbz{c}"),
                    format!("V{c}{c}OL1.CBZ"),
                ] {
                    assert_eq!(fold_name(&name), "vol1.cbz", "U+{cp:04X} in {name:?}");
                }
                assert_eq!(fold_name(&c.to_string()), "", "U+{cp:04X} alone");
            }
            for edge in [lo.checked_sub(1), hi.checked_add(1)].into_iter().flatten() {
                if let Some(c) = char::from_u32(edge)
                    && !super::is_default_ignorable(c)
                {
                    assert_ne!(fold_name(&format!("a{c}b")), "ab", "U+{edge:04X}");
                }
            }
        }
        // UCD 15.0.0 lists 4174 (Linux casefold, on Unicode 12.1, drops 4173 of them:
        // all but U+180F, which 12.1 does not have).
        assert_eq!(count, 4174);
        // The names the verifier reported: zero width space, ZWJ, LRM, a variation
        // selector, word joiner, CGJ, Hangul filler.
        for c in [
            '\u{200B}', '\u{200D}', '\u{200E}', '\u{FE0F}', '\u{2060}', '\u{034F}', '\u{3164}',
        ] {
            assert_eq!(fold_name(&format!("Kingdom{c}")), fold_name("KINGDOM"));
        }
        // A grapheme joiner between a letter and its accent: the accent still joins.
        assert_eq!(fold_name("E\u{34F}\u{301}"), fold_name("\u{e9}"));
        // Visible format characters that are not default-ignorable stay.
        assert_ne!(fold_name("a\u{0600}b"), fold_name("ab"));
        assert_ne!(fold_name("a\u{00A0}b"), fold_name("ab"));
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
            "V\u{200B}ol\u{FE0F}",
            "E\u{34F}\u{301}",
        ] {
            let once = fold_name(name);
            assert_eq!(fold_name(&once), once, "{name:?}");
        }
    }
}
