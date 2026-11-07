//! How released Mokuro Reader (1.10.0 and earlier, "R0") reads sidecar names, for
//! doctor's reports only. Nothing here decides anything: Bunko reads, writes, governs
//! and sweeps every name by rule 2 ([`super`]).
//!
//! R0 is `classifyMokuroSidecar` as v1.10.0 ships it: `.cbz`, `.mokuro` and
//! `.mokuro.gz` are matched on the name's Unicode lowercase and cut off the name by
//! length; stems are compared lowercased, without NFC; a layer id is the lowercased last
//! segment of the base, matched against `^[a-z0-9-]{1,32}$`. Lowercase joins less than
//! fold v2 (every pair it joins, the fold joins), so R0 attributes a subset of what
//! rule 2 does, with one exception: archives `A` and `B` where `B`'s stem folds like
//! `A`'s stem plus `.<id>` but does not lowercase like it (an NFD `Cafe\u{301}.fix.cbz`
//! beside `Café.cbz`): R0 shows `Café.fix.mokuro` as `A`'s layer `fix`, rule 2 as `B`'s
//! primary ([`DiffKind::Misread`]). Every other difference is a file rule 2 attributes
//! and R0 does not see at all ([`DiffKind::Unseen`]: `STRASSE.mokuro` beside
//! `Straße.cbz`).

use super::{Folder, SidecarClass, classify, fold};

/// What one reading makes of a `.mokuro` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading<'a> {
    /// The primary of the listed archive `archive`.
    Primary { archive: &'a str },
    /// Layer `id` of the listed archive `archive`.
    Layer { archive: &'a str, id: String },
    /// No archive's.
    Orphan,
}

impl Reading<'_> {
    /// The same attribution, an archive's spellings of one fold being one archive.
    fn agrees(&self, other: &Reading<'_>) -> bool {
        match (self, other) {
            (Reading::Orphan, Reading::Orphan) => true,
            (Reading::Primary { archive: a }, Reading::Primary { archive: b }) => {
                fold(a) == fold(b)
            }
            (Reading::Layer { archive: a, id: x }, Reading::Layer { archive: b, id: y }) => {
                x == y && fold(a) == fold(b)
            }
            _ => false,
        }
    }
}

/// `raw` without its last `units` UTF-16 code units (JavaScript's `slice(0, -n)`), or
/// `None` when that would split a character.
fn cut_utf16(raw: &str, units: usize) -> Option<&str> {
    let mut left = units;
    for (i, c) in raw.char_indices().rev() {
        if left == 0 {
            return Some(&raw[..i + c.len_utf8()]);
        }
        left = left.checked_sub(c.len_utf16())?;
    }
    (left == 0).then_some("")
}

fn r0_mokuro(name: &str) -> Option<(&str, bool)> {
    let lower = name.to_lowercase();
    if lower.ends_with(".mokuro.gz") {
        return cut_utf16(name, 10).map(|base| (base, true));
    }
    if lower.ends_with(".mokuro") {
        return cut_utf16(name, 7).map(|base| (base, false));
    }
    None
}

fn r0_cbz_stem(name: &str) -> Option<&str> {
    if name.to_lowercase().ends_with(".cbz") {
        cut_utf16(name, 4)
    } else {
        None
    }
}

fn is_r0_layer_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// How Mokuro Reader 1.10.0 reads the `.mokuro` name `name` in a folder whose listed
/// files are `names`. R0 pairs by lowercased stem alone; where several listed archives
/// lowercase alike, the one spelled exactly like the stem, else the smallest, stands
/// for them.
pub fn r0_classify<'a>(name: &str, names: &[&'a str]) -> Reading<'a> {
    let archives: Vec<(&'a str, &'a str, String)> = names
        .iter()
        .filter_map(|&n| r0_cbz_stem(n).map(|stem| (n, stem, stem.to_lowercase())))
        .collect();
    let pick = |stem: &str| -> Option<&'a str> {
        let lower = stem.to_lowercase();
        let found: Vec<&(&'a str, &'a str, String)> =
            archives.iter().filter(|(_, _, l)| *l == lower).collect();
        found
            .iter()
            .find(|(_, raw, _)| *raw == stem)
            .or_else(|| found.iter().min())
            .map(|(archive, _, _)| *archive)
    };
    let Some((base, _)) = r0_mokuro(name) else {
        return Reading::Orphan;
    };
    if let Some(archive) = pick(base) {
        return Reading::Primary { archive };
    }
    let Some(dot) = base.rfind('.').filter(|&dot| dot > 0) else {
        return Reading::Orphan;
    };
    let id = base[dot + 1..].to_lowercase();
    if !is_r0_layer_id(&id) {
        return Reading::Orphan;
    }
    match pick(&base[..dot]) {
        Some(archive) => Reading::Layer { archive, id },
        None => Reading::Orphan,
    }
}

/// How rule 2 reads the `.mokuro` name `name` of `folder`, the archive as it is shown
/// ([`Folder::display_archive`]).
pub fn rule2_reading<'a>(name: &str, folder: &Folder<'a>) -> Reading<'a> {
    let archive = || folder.display_archive(name);
    match classify(name, &folder.volume_stems()) {
        SidecarClass::Primary { .. } => {
            archive().map_or(Reading::Orphan, |archive| Reading::Primary { archive })
        }
        SidecarClass::Layer { id, .. } => {
            archive().map_or(Reading::Orphan, |archive| Reading::Layer {
                archive,
                id: id.into_owned(),
            })
        }
        SidecarClass::Orphan => Reading::Orphan,
    }
}

/// How a released reader's view of a name differs from rule 2's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// Rule 2 attributes the file; R0 calls it an orphan. Harmless: R0 just does not
    /// show it (respelling the file as its archive is spelled makes it visible).
    Unseen,
    /// R0 shows the file as a layer of one archive, rule 2 as the primary of another
    /// (a crafted or hand-made nested archive). Renaming that archive clears it.
    Misread,
}

/// One listed name released readers read differently from rule 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDiff<'a> {
    pub name: &'a str,
    pub rule2: Reading<'a>,
    pub r0: Reading<'a>,
}

impl LegacyDiff<'_> {
    pub fn kind(&self) -> DiffKind {
        if self.r0 == Reading::Orphan {
            DiffKind::Unseen
        } else {
            DiffKind::Misread
        }
    }
}

/// Every listed `.mokuro` of a folder whose listed files are `names` that Mokuro Reader
/// 1.10.0 reads otherwise than rule 2, in listing order (doctor: "Released readers read
/// these differently").
pub fn r0_differences<'a>(names: &[&'a str]) -> Vec<LegacyDiff<'a>> {
    let folder = Folder::new(names);
    names
        .iter()
        .filter(|n| r0_mokuro(n).is_some() || super::split_mokuro(n).is_some())
        .filter_map(|&name| {
            let rule2 = rule2_reading(name, &folder);
            let r0 = r0_classify(name, names);
            (!rule2.agrees(&r0)).then_some(LegacyDiff { name, rule2, r0 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r0_reads_as_released_readers_do() {
        let v = ["Vol 1.cbz"];
        assert_eq!(
            r0_classify("Vol 1.MOKURO", &v),
            Reading::Primary {
                archive: "Vol 1.cbz"
            }
        );
        // 1.10.0 lowercases ids, as rule 2 folds them.
        assert_eq!(
            r0_classify("Vol 1.EDIT.mokuro", &v),
            Reading::Layer {
                archive: "Vol 1.cbz",
                id: "edit".into()
            }
        );
        assert_eq!(r0_classify("Vol 1.Bad_Id.mokuro", &v), Reading::Orphan);
        assert_eq!(r0_classify(".x.mokuro", &[".cbz"]), Reading::Orphan);
        // No NFC, no full case folding.
        assert_eq!(
            r0_classify("STRASSE.mokuro", &["Stra\u{df}e.cbz"]),
            Reading::Orphan
        );
        assert_eq!(
            r0_classify("Cafe\u{301}.mokuro", &["Caf\u{e9}.cbz"]),
            Reading::Orphan
        );
        // A suffix only its Unicode lowercase shows (a Kelvin sign) is still one.
        assert_eq!(
            r0_classify("V.mo\u{212A}uro", &["V.cbz"]),
            Reading::Primary { archive: "V.cbz" }
        );
    }

    #[test]
    fn differences_are_unseen_files_and_misread_nested_pairs() {
        let listing = [
            "Stra\u{df}e.cbz",
            "STRASSE.mokuro",
            "Caf\u{e9}.cbz",
            "Cafe\u{301}.mokuro",
            "V.cbz",
            "V.mokuro",
            "V.EDIT.mokuro",
        ];
        let diffs = r0_differences(&listing);
        let got: Vec<(&str, DiffKind)> = diffs.iter().map(|d| (d.name, d.kind())).collect();
        assert_eq!(
            got,
            [
                ("STRASSE.mokuro", DiffKind::Unseen),
                ("Cafe\u{301}.mokuro", DiffKind::Unseen),
            ]
        );
        // A crafted nested archive: 1.10.0 shows its main as the other's layer.
        let listing = [
            "Caf\u{e9}.cbz",
            "Cafe\u{301}.fix.cbz",
            "Caf\u{e9}.fix.mokuro",
        ];
        let diffs = r0_differences(&listing);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].kind(), DiffKind::Misread);
        assert_eq!(
            diffs[0].r0,
            Reading::Layer {
                archive: "Caf\u{e9}.cbz",
                id: "fix".into()
            }
        );
        assert_eq!(
            diffs[0].rule2,
            Reading::Primary {
                archive: "Cafe\u{301}.fix.cbz"
            }
        );
    }

    /// Whenever R0 attributes a file to an archive, rule 2 attributes it to the same
    /// archive (or its fold group), but for the documented nested pair, which
    /// [`r0_differences`] reports as misread.
    #[test]
    fn rule_2_attributes_everything_released_readers_do() {
        let stems = [
            "V",
            "v",
            "Caf\u{e9}",
            "Cafe\u{301}",
            "Stra\u{df}e",
            "STRASSE",
            "\u{39f}\u{394}\u{39f}\u{3a3}",
            "\u{3bf}\u{3b4}\u{3bf}\u{3c2}",
            "\u{130}",
            "i\u{307}",
        ];
        let mut archives = Vec::new();
        let mut files = Vec::new();
        for stem in stems {
            for tail in ["", ".fix", ".5"] {
                archives.push(format!("{stem}{tail}.cbz"));
            }
            for tail in [".mokuro", ".fix.mokuro", ".FIX.mokuro", ".5.mokuro.gz"] {
                files.push(format!("{stem}{tail}"));
            }
        }
        let mut checked = 0;
        for (i, a) in archives.iter().enumerate() {
            for b in &archives[i..] {
                for file in &files {
                    let listing = [a.as_str(), b.as_str(), file.as_str()];
                    let r0 = r0_classify(file, &listing);
                    if r0 == Reading::Orphan {
                        continue;
                    }
                    checked += 1;
                    let rule2 = rule2_reading(file, &Folder::new(&listing));
                    if rule2.agrees(&r0) {
                        continue;
                    }
                    let (Reading::Layer { archive, id }, Reading::Primary { archive: nested }) =
                        (&r0, &rule2)
                    else {
                        panic!("{file:?} in {listing:?}: R0 {r0:?}, rule 2 {rule2:?}");
                    };
                    // `B`'s stem folds like `A`'s plus the id, and like the file's base,
                    // but does not lowercase like the file's base.
                    let base = super::super::cbz_stem(archive).unwrap();
                    let nested = super::super::cbz_stem(nested).unwrap();
                    let (file_base, _) = r0_mokuro(file).unwrap();
                    assert_eq!(fold(nested), format!("{}.{id}", fold(base)));
                    assert_eq!(fold(nested), fold(file_base));
                    assert_ne!(nested.to_lowercase(), file_base.to_lowercase());
                    assert!(
                        r0_differences(&listing)
                            .iter()
                            .any(|d| d.name == file && d.kind() == DiffKind::Misread)
                    );
                }
            }
        }
        assert!(checked > 1000, "{checked}");
    }
}
