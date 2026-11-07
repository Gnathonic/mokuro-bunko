//! The one fold: fold v2 (UCD 17.0.0), which every name question is decided by.
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
//! Sidecar names rule 2 (which files are a volume's, the contract with Mokuro Reader:
//! [`crate::sidecar_names`]) decides on this same fold, so what is one entry and what
//! belongs to a volume never disagree. The tables are generated
//! (`crates/bunko-core/scripts/gen_fold.py`) and shared byte for byte with the reader
//! through `interop/contract/fold-table.json`; `interop/contract/fold.json` pins the
//! result with vectors and a digest over every scalar value.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

use crate::fold_table::{DEFAULT_IGNORABLE, FOLD_MAP};

pub use crate::fold_table::FOLD_UCD;

/// The fold's version. A change of it (a UCD bump included) is a new sidecar names
/// rule, landed in Bunko and the reader at once.
pub const FOLD_VERSION: u32 = 2;

/// Is `c` a Default_Ignorable_Code_Point (UCD [`FOLD_UCD`], [`DEFAULT_IGNORABLE`])?
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

/// `M(c) = casefold(full_upper(casefold(c)))`, appended to `out`.
fn push_mapped(out: &mut String, c: char) {
    match FOLD_MAP.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
        Ok(i) => out.push_str(FOLD_MAP[i].1),
        Err(_) => out.push(c),
    }
}

/// Fold v2, the form two names that are one entry share. An ASCII name folds to its
/// ASCII lowercase. Any other name: the default-ignorable code points dropped,
/// canonical decomposition, each code point mapped to the case folding of the
/// uppercase of its case folding (so a letter an uppercase table joins with another,
/// `ı` with `i`, folds alike too), recomposed (NFC), the default-ignorable code points
/// dropped again. Context-free and idempotent; no code point folds to or from `.` or
/// `/`, so `F(a.b) = F(a).F(b)`.
pub fn fold_name(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    let mut mapped = String::with_capacity(name.len());
    for c in name.chars().filter(|c| !is_default_ignorable(*c)).nfd() {
        push_mapped(&mut mapped, c);
    }
    mapped.nfc().filter(|c| !is_default_ignorable(*c)).collect()
}

/// [`fold_name`], borrowing `name` when it is its own fold (ASCII without an
/// uppercase letter).
pub fn folded(name: &str) -> Cow<'_, str> {
    if name.is_ascii() && !name.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(fold_name(name))
    }
}

/// Is `name` the fixed name `fixed` (`series.json`, `catalog.json`, a per-user name)
/// in some spelling: one entry with it by [`fold_name`]? Every name the server treats
/// specially is recognised this way, so `ſeries.json`, `SERIES.JSON` and
/// `series\u{200B}.json`, which every host makes one file, are `series.json` everywhere
/// it is checked.
pub fn is_fixed_name(name: &str, fixed: &str) -> bool {
    folded(name) == folded(fixed)
}

/// The whitespace of [`series_key`], asked after the fold: Python's `str.isspace()`
/// united with JavaScript's `\s`.
pub fn is_series_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{D}'
            | '\u{1C}'..='\u{1F}'
            | ' '
            | '\u{85}'
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

/// The key a series is known by (its folder's owner records, the adoption grouping):
/// [`fold_name`], then trimmed, then every run of whitespace ([`is_series_whitespace`])
/// one space. A function of the fold, so never finer than it: `Straße` and `STRASSE`,
/// `Dr Stone` and `Dr  Stone` are one series. `interop/contract/fold.json` pins it.
pub fn series_key(name: &str) -> String {
    let folded = fold_name(name);
    let mut out = String::with_capacity(folded.len());
    let mut pending = false;
    for c in folded.chars() {
        if is_series_whitespace(c) {
            pending = !out.is_empty();
            continue;
        }
        if pending {
            out.push(' ');
            pending = false;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{fold_name, series_key};
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    fn contract(file: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../interop/contract")
            .join(file);
        let text = std::fs::read_to_string(&path).expect("interop/contract file");
        serde_json::from_str(&text).expect("valid JSON")
    }

    fn s(v: &Value) -> &str {
        v.as_str().unwrap()
    }

    /// `interop/contract/fold.json`: every vector, same and different pair, and
    /// series_key vector, as the reader runs them.
    #[test]
    fn fold_contract() {
        let v = contract("fold.json");
        assert_eq!(v["fold"], super::FOLD_VERSION);
        assert_eq!(v["ucd"], super::FOLD_UCD);
        let vectors = v["vectors"].as_array().unwrap();
        assert!(vectors.len() >= 50);
        for case in vectors {
            assert_eq!(
                fold_name(s(&case["in"])),
                s(&case["out"]),
                "{}",
                case["why"]
            );
        }
        for case in v["same"].as_array().unwrap() {
            assert_eq!(
                fold_name(s(&case["a"])),
                fold_name(s(&case["b"])),
                "{}",
                case["why"]
            );
        }
        for case in v["different"].as_array().unwrap() {
            assert_ne!(
                fold_name(s(&case["a"])),
                fold_name(s(&case["b"])),
                "{}",
                case["why"]
            );
        }
        for case in v["series_key"].as_array().unwrap() {
            assert_eq!(
                series_key(s(&case["in"])),
                s(&case["out"]),
                "{}",
                case["why"]
            );
        }
    }

    /// The table the reader bundles is the one this build reads: `table_sha256` names
    /// its bytes, and its case map decodes (the contract README's grammar) to
    /// [`crate::fold_table::FOLD_MAP`] entry for entry.
    #[test]
    fn the_contract_table_is_the_generated_one() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../interop/contract/fold-table.json");
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            s(&contract("fold.json")["table_sha256"])
        );
        let table: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(table["fold"], super::FOLD_VERSION);
        assert_eq!(table["ucd"], super::FOLD_UCD);
        let mut prev = 0u32;
        let mut decoded = Vec::new();
        for entry in s(&table["map"]).split(';') {
            let (delta, targets) = entry.split_once(':').unwrap();
            let cp = prev + u32::from_str_radix(delta, 16).unwrap();
            let to: String = targets
                .split(',')
                .map(|t| {
                    let off = match t.strip_prefix('-') {
                        Some(n) => -i64::from_str_radix(n, 16).unwrap(),
                        None => i64::from_str_radix(t, 16).unwrap(),
                    };
                    char::from_u32(u32::try_from(i64::from(cp) + off).unwrap()).unwrap()
                })
                .collect();
            decoded.push((cp, to));
            prev = cp;
        }
        let ours: Vec<(u32, String)> = crate::fold_table::FOLD_MAP
            .iter()
            .map(|&(cp, to)| (cp, to.to_string()))
            .collect();
        assert_eq!(decoded, ours);
        let mut prev = 0u32;
        let mut ranges = Vec::new();
        for entry in s(&table["ignorable"]).split(';') {
            let (gap, len) = entry.split_once(':').unwrap();
            let lo = prev + u32::from_str_radix(gap, 16).unwrap();
            let hi = lo + u32::from_str_radix(len, 16).unwrap() - 1;
            ranges.push((lo, hi));
            prev = hi + 1;
        }
        assert_eq!(ranges, crate::fold_table::DEFAULT_IGNORABLE);
    }

    /// `interop/contract/fold.json`'s digest: F(c), F('a' + c), F(c + U+0301) for
    /// every Unicode scalar value, each UTF-8 and LF-terminated, hashed with SHA-256.
    /// The reader reproduces it with its own bundled normalizer.
    #[test]
    fn fold_digest_over_every_scalar_value() {
        let want = s(&contract("fold.json")["digest"]["sha256"]).to_string();
        let mut h = Sha256::new();
        let mut probe = String::new();
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            for (pre, post) in [(None, None), (Some('a'), None), (None, Some('\u{301}'))] {
                probe.clear();
                probe.extend(pre);
                probe.push(c);
                probe.extend(post);
                h.update(fold_name(&probe).as_bytes());
                h.update(b"\n");
            }
        }
        assert_eq!(hex::encode(h.finalize()), want);
    }

    /// unicode-normalization is pinned (`=0.1.25`): its NFD/NFC is the fold's UCD.
    #[test]
    fn the_normalization_crate_is_on_the_folds_ucd() {
        let (a, b, c) = unicode_normalization::UNICODE_VERSION;
        assert_eq!(format!("{a}.{b}.{c}"), super::FOLD_UCD);
    }

    #[test]
    fn fixed_names_are_recognised_in_every_spelling_the_fold_joins() {
        for name in [
            "series.json",
            "Series.JSON",
            "\u{17F}eries.json",
            "series\u{200B}.json",
            "SER\u{AD}IES.json",
        ] {
            assert!(super::is_fixed_name(name, "series.json"), "{name:?}");
        }
        for name in ["series.json.bak", "series_json", "serie.json", "seriesjson"] {
            assert!(!super::is_fixed_name(name, "series.json"), "{name:?}");
        }
    }

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

    /// Every Default_Ignorable_Code_Point (the generated UCD 17.0.0 table) is dropped,
    /// wherever it stands in a name, as Linux casefold drops it: a name that differs
    /// from another only by such code points is that name. And only those are: the
    /// characters around each range still count.
    #[test]
    fn default_ignorable_code_points_are_no_part_of_a_name() {
        use crate::fold_table::DEFAULT_IGNORABLE;
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
        // UCD 17.0.0 lists 4174, as 15.0.0 did (Linux casefold, on Unicode 12.1, drops
        // 4173 of them: all but U+180F, which 12.1 does not have).
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

    #[test]
    fn series_keys_are_the_fold_trimmed_with_whitespace_collapsed() {
        assert_eq!(series_key("Stra\u{df}e"), series_key("STRASSE"));
        assert_eq!(series_key("Dr Stone"), series_key("Dr  Stone"));
        assert_eq!(series_key("  Dr\u{3000}Stone\t"), "dr stone");
        assert_eq!(series_key("Dr\u{200B} Stone"), "dr stone");
        assert_ne!(series_key("Dr Stone"), series_key("DrStone"));
    }
}
