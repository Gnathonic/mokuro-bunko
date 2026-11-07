//! What a file beside a volume archive IS: sidecar names rule 2, shared with
//! mokuro-reader (`classifyMokuroSidecar` in `src/lib/util/sync/syncable-file.ts`).
//! Rule 2 decides on the one fold, fold v2 ([`fold`], [`crate::names`]): let
//! `f = F(name)`. F keeps every `.`, so the k-th dot of a name is the k-th dot of its
//! fold, and every slice of a name is cut at the same dot as its fold's.
//!
//! * R1 (entries): two names are one entry iff they fold alike.
//! * R2 (suffixes): a name's suffix is the first of `.cbz`, `.mokuro.gz`, `.mokuro`,
//!   `.webp`, `.nocover` its fold ends with ([`suffix_of`]). Only `.cbz` archives
//!   define volume stems: the folds of the listed `.cbz` stems ([`volume_stems`]). The
//!   other [`ARCHIVE_EXTENSIONS`] still keep a sweep off their files.
//! * R3 (primary): a `.mokuro[.gz]` whose folded base is a volume stem is that stem's
//!   primary.
//! * R4: a primary reading wins over a layer reading (`Vol 1.5.mokuro` is volume 1.5's
//!   when `Vol 1.5.cbz` is listed).
//! * R5 (layer): otherwise, when its folded base is `<s>.<id>`, `<s>` a volume stem and
//!   `<id>` matching `^[a-z0-9-]{1,32}$` ([`generations::is_layer_id`]), it is layer
//!   `<id>` of `<s>`. The id is the FOLDED last segment and is reported folded
//!   (`V.EDIT.mokuro` and `V.ed\u{200B}it.mokuro` are layer `edit`); all-digit ids are
//!   layers. Otherwise it is an orphan. Layer classes, nested ids and the hidden-layer
//!   pass-up all read folded ids.
//! * R6 (which archive): of the listed archives whose folded stem is `<s>`, the one
//!   spelled exactly like the file's stem, else the only one. Otherwise (a fold group,
//!   which only a store holding case variants has, and only until they are merged): to
//!   read or show, the code-point-smallest spelling ([`Folder::display_archive`]); to
//!   sweep, delete or rename, exact spellings only ([`Folder::volume_files`]).
//! * R7: a leading UTF-8 BOM is tolerated on read and never written; hashes cover the
//!   raw bytes (`bunko_library::pyjson::strip_bom`).
//! * R8: fixed names (`series.json`, `catalog.json`, `volume-data.json`,
//!   `.mokuro-queue.json`, `.mokuro-bunko.json`, the per-user root names) are
//!   recognised by their fold ([`crate::names::is_fixed_name`]).
//! * R9 (writers): a writer writes `<listed archive stem>.<id>.mokuro`, a minted id
//!   lowercase ASCII with a letter, onto the listed entry of the same fold rather than a
//!   new spelling.
//!
//! A bare file name cannot tell a decimal volume's primary from a layer, so every
//! decision here takes the folder listing. Every decision reads only the folds of the
//! name and of the listing, and maps its answer back to listed spellings, so a crafted
//! spelling decides exactly as its canonical twin (`Q.ed\u{200B}it.edit.cbz` as
//! `q.edit.edit.cbz`): one fold, nothing left for two folds to disagree about. Both
//! sides run the vectors in `interop/contract/sidecar-names.json` (rule 2) and
//! `interop/contract/fold.json`. Rule v1 (stems NFC-then-lowercased, ids exact) is
//! retired; how released readers read names is [`legacy`]'s, for doctor only.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::generations::is_layer_id;
use crate::names::folded;

pub mod legacy;

#[cfg(test)]
mod commutation;

/// The sidecar names rule this build implements (`interop/contract/sidecar-names.json`).
pub const SIDECAR_RULE: u32 = 2;

/// Archive extensions a volume can have. Only `.cbz` anchors sidecars (R2); the others
/// still keep a sweep off their files ([`sweep_set`]) and count as volumes where a
/// volume is all that is asked (the storage Check, failure records).
pub const ARCHIVE_EXTENSIONS: [&str; 4] = [".cbz", ".cbr", ".zip", ".rar"];

/// The one fold: which names are one entry ([`crate::names::fold_name`]), and what
/// every rule 2 decision reads. Every comparison of names as paths (PathCase, the case
/// variants, the owner records, the write locks, a cloud index) folds with it.
pub use crate::names::fold_name as fold;

/// A volume-file suffix (R2), as [`suffix_of`] reads it from a name's fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Suffix {
    Cbz,
    MokuroGz,
    Mokuro,
    Webp,
    NoCover,
}

impl Suffix {
    /// Every suffix, in the order R2 asks them.
    pub const ALL: [Suffix; 5] = [
        Suffix::Cbz,
        Suffix::MokuroGz,
        Suffix::Mokuro,
        Suffix::Webp,
        Suffix::NoCover,
    ];

    /// The suffix as written in a fold: `.cbz`, `.mokuro.gz`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            Suffix::Cbz => ".cbz",
            Suffix::MokuroGz => ".mokuro.gz",
            Suffix::Mokuro => ".mokuro",
            Suffix::Webp => ".webp",
            Suffix::NoCover => ".nocover",
        }
    }
}

/// A name and its fold, cut at the same dots.
#[derive(Debug, Clone)]
struct Folded<'a> {
    raw: &'a str,
    fold: Cow<'a, str>,
}

impl<'a> Folded<'a> {
    fn new(raw: &'a str) -> Self {
        Folded {
            raw,
            fold: folded(raw),
        }
    }

    /// The name and its fold before their `n`-th dot from the end (1: the last).
    fn before_dot_from_end(&self, n: usize) -> Option<(&'a str, &str)> {
        let n = n.checked_sub(1)?;
        let raw = self.raw.rmatch_indices('.').nth(n)?.0;
        let fold = self.fold.rmatch_indices('.').nth(n)?.0;
        Some((&self.raw[..raw], &self.fold[..fold]))
    }

    /// The name and its fold with `suffix` (as folded: lowercase, its dots included)
    /// cut off, when the fold ends with it.
    fn strip(&self, suffix: &str) -> Option<(&'a str, &str)> {
        if !self.fold.ends_with(suffix) {
            return None;
        }
        self.before_dot_from_end(suffix.matches('.').count())
    }

    /// `part`, a slice of the fold, borrowing the name when the name is its own fold.
    fn fold_part(&self, part: &str) -> Cow<'a, str> {
        match &self.fold {
            Cow::Borrowed(whole) => {
                let start = part.as_ptr() as usize - whole.as_ptr() as usize;
                Cow::Borrowed(&whole[start..start + part.len()])
            }
            Cow::Owned(_) => Cow::Owned(part.to_string()),
        }
    }

    /// `(raw base, folded base, gz)` of a `.mokuro[.gz]` name.
    fn mokuro(&self) -> Option<(&'a str, &str, bool)> {
        if let Some((raw, fold)) = self.strip(".mokuro.gz") {
            return Some((raw, fold, true));
        }
        self.strip(".mokuro").map(|(raw, fold)| (raw, fold, false))
    }
}

/// `(raw stem, folded stem, folded id)` of a base `<stem>.<id>`, split on its LAST dot,
/// when the folded id is a layer id and the folded stem is not empty.
fn split_base<'b, 'f>(raw: &'b str, fold: &'f str) -> Option<(&'b str, &'f str, &'f str)> {
    let (fold_stem, id) = fold.rsplit_once('.')?;
    let (raw_stem, _) = raw.rsplit_once('.')?;
    (!fold_stem.is_empty() && is_layer_id(id)).then_some((raw_stem, fold_stem, id))
}

/// The volume-file suffix `name` ends in once folded (R2), or none.
pub fn suffix_of(name: &str) -> Option<Suffix> {
    let fold = folded(name);
    Suffix::ALL
        .into_iter()
        .find(|suffix| fold.ends_with(suffix.as_str()))
}

/// The names among `names` that another listed name shadows: in each group of two or
/// more names that [`fold`] alike, every name but the winner. The winner is
/// `preferred(fold)` when that name is listed (a remote store's case winner, the
/// spelling its case merge keeps), else the byte-smallest (code-point order). This is
/// the rule PathCase resolves every request spelling by, so a shadowed name is one no
/// request reaches: listings, the catalog and the library walks leave it out.
pub fn case_losers<'a>(
    names: impl IntoIterator<Item = &'a str>,
    preferred: impl Fn(&str) -> Option<String>,
) -> HashSet<String> {
    let mut groups: HashMap<String, Vec<&str>> = HashMap::new();
    for name in names {
        groups.entry(fold(name)).or_default().push(name);
    }
    let mut out = HashSet::new();
    for (folded, group) in groups {
        if group.len() < 2 {
            continue;
        }
        let winner = preferred(&folded)
            .filter(|w| group.contains(&w.as_str()))
            .or_else(|| group.iter().min().map(|n| n.to_string()));
        out.extend(
            group
                .into_iter()
                .filter(|n| Some(*n) != winner.as_deref())
                .map(str::to_string),
        );
    }
    out
}

/// The names of a folder listing no request reaches, `entries` being every listed
/// `(name, is_dir)`: the [`case_losers`], and every file rule 2 gives to a shadowed
/// `.cbz` archive ([`Folder::volume_files`] over the whole listing: the sidecars, cover,
/// no-cover marker and layers named by its exact stem). A hidden archive's own files
/// stay its own: never shown, read, written, moved or deleted through the archive that
/// shadows it, until the case variants are merged.
pub fn hidden_names<'a>(
    entries: impl IntoIterator<Item = (&'a str, bool)>,
    preferred: impl Fn(&str) -> Option<String>,
) -> HashSet<String> {
    crate::fold_groups::FoldGroups::new(entries, preferred)
        .hidden()
        .clone()
}

/// `name` minus `suffix`, the suffix matched in any ASCII case, as written. Rule 2
/// reads suffixes on the fold ([`strip_suffix_folded`]); this is for names Bunko
/// spells itself.
pub fn strip_suffix_ci<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = name.len().checked_sub(suffix.len())?;
    (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(suffix)).then(|| &name[..cut])
}

/// `name` before `suffix` (an ASCII suffix starting with a dot: `.cbz`, `.mokuro.gz`,
/// `.zip`) when `name`'s fold ends with it (R2), cut at the same dot:
/// `V.cb\u{200B}z` is `V` + `.cbz`.
pub fn strip_suffix_folded<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let suffix = suffix.to_ascii_lowercase();
    Folded::new(name).strip(&suffix).map(|(raw, _)| raw)
}

/// `Vol 1` for `Vol 1.cbz` (any spelling the fold reads as `.cbz`); `None` for any
/// other name.
pub fn cbz_stem(name: &str) -> Option<&str> {
    Folded::new(name).strip(".cbz").map(|(raw, _)| raw)
}

/// `(base, gz)` of `<base>.mokuro` / `<base>.mokuro.gz` (on the fold), the base as
/// written.
pub fn split_mokuro(name: &str) -> Option<(&str, bool)> {
    Folded::new(name).mokuro().map(|(raw, _, gz)| (raw, gz))
}

/// `(stem, id, gz)` of `<stem>.<id>.mokuro[.gz]`, split on the LAST dot: the stem as
/// written, the id FOLDED (R5: `V.EDIT.mokuro` is `("V", "edit", false)`).
/// Listing-free: says nothing about whether `<stem>` is a volume, nor whether the
/// whole name is a decimal volume's primary — [`classify`] decides that.
pub fn split_layer(name: &str) -> Option<(&str, Cow<'_, str>, bool)> {
    let name = Folded::new(name);
    let (raw, fold, gz) = name.mokuro()?;
    let (stem, _, id) = split_base(raw, fold)?;
    Some((stem, name.fold_part(id), gz))
}

/// What a listed `.mokuro` is. `stem` is spelled as in the name, not as the archive;
/// `id` is folded (R5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarClass<'a> {
    Primary {
        stem: &'a str,
        gz: bool,
    },
    Layer {
        stem: &'a str,
        id: Cow<'a, str>,
        gz: bool,
    },
    /// No archive here owns it, or it is no `.mokuro` at all.
    Orphan,
}

/// The folded stems of the `.cbz` archives among `names` (R2).
pub fn volume_stems<'a>(names: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    names
        .into_iter()
        .filter_map(|name| {
            Folded::new(name)
                .strip(".cbz")
                .map(|(_, fold)| fold.to_string())
        })
        .collect()
}

/// Rule 2 for one `.mokuro` reading: `(raw stem, folded stem, folded id, gz)`, the id
/// `None` for a primary.
fn reading<'n, 'a>(
    name: &'n Folded<'a>,
    folded_cbz_stems: &HashSet<String>,
) -> Option<(&'a str, &'n str, Option<&'n str>, bool)> {
    let (raw, fold, gz) = name.mokuro()?;
    if folded_cbz_stems.contains(fold) {
        return Some((raw, fold, None, gz));
    }
    let (raw_stem, fold_stem, id) = split_base(raw, fold)?;
    folded_cbz_stems
        .contains(fold_stem)
        .then_some((raw_stem, fold_stem, Some(id), gz))
}

/// Rule 2 for one name, given the folder's [`volume_stems`].
pub fn classify<'a>(name: &'a str, folded_cbz_stems: &HashSet<String>) -> SidecarClass<'a> {
    let name = Folded::new(name);
    match reading(&name, folded_cbz_stems) {
        Some((stem, _, None, gz)) => SidecarClass::Primary { stem, gz },
        Some((stem, _, Some(id), gz)) => SidecarClass::Layer {
            stem,
            id: name.fold_part(id),
            gz,
        },
        None => SidecarClass::Orphan,
    }
}

/// Who may write a layer id. Read on the folded id (R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerClass {
    /// `edit`, `edit-<n>`: one account's own edits.
    Personal,
    /// `curated`: the shared edit, published by accounts with the right to.
    Curated,
    /// Reserved, but no client writes it here: `curated-<n>`.
    Reserved,
    /// Everything else: engine generations, `original`, `gcv`, `updated-ocr`,
    /// `tr-*`, hand-made ids — governed as before.
    Other,
}

/// `<prefix>` or `<prefix>-<digits>`.
fn is_counted(id: &str, prefix: &str) -> bool {
    match id.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

/// The personal layer id (D27): `edit` and `edit-<n>` are each account's own.
pub const PERSONAL_LAYER: &str = "edit";

/// The layer ids a server keeps per account, as `/login/api/me` lists them
/// (`per_user_layers`; each covers `<id>-<n>` too). A reader pushes its corrections
/// only when its id is listed.
pub fn per_user_layers() -> Vec<String> {
    vec![PERSONAL_LAYER.to_string()]
}

/// The [`LayerClass`] of a layer id, read on its fold (R5): `EDIT` and `ed\u{200B}it`
/// are `edit`'s.
pub fn layer_class(id: &str) -> LayerClass {
    let id = folded(id);
    if is_counted(&id, PERSONAL_LAYER) {
        LayerClass::Personal
    } else if id == "curated" {
        LayerClass::Curated
    } else if is_counted(&id, "curated") {
        LayerClass::Reserved
    } else {
        LayerClass::Other
    }
}

/// The [`LayerClass`] of a listed name when it is a layer, else `None`.
pub fn layer_class_of_name(name: &str, folded_cbz_stems: &HashSet<String>) -> Option<LayerClass> {
    match classify(name, folded_cbz_stems) {
        SidecarClass::Layer { id, .. } => Some(layer_class(&id)),
        _ => None,
    }
}

/// One file of a volume, as [`volume_files`] finds it. A layer's id is folded (R5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeFile<'a> {
    Primary { gz: bool },
    Cover,
    NoCover,
    Layer { id: Cow<'a, str>, gz: bool },
}

/// A folder listing prepared once for every archive in it (a compile pass, the OCR
/// walk and the catalog index ask about each volume of a folder): every name folded
/// once, the archive stems, and the folded stems its `.mokuro` names could belong to,
/// so an archive none of them folds to is answered without walking the listing.
pub struct Folder<'a> {
    names: Vec<Folded<'a>>,
    /// The folded stems of every archive of [`ARCHIVE_EXTENSIONS`], with how many
    /// spellings fold to each.
    stem_spellings: HashMap<String, usize>,
    /// The listed `.cbz` archives: `(name, raw stem, folded stem)`.
    cbz: Vec<(&'a str, &'a str, String)>,
    /// Folded bases of the `.mokuro[.gz]` names, and of their layer readings' stems.
    sidecar_folds: HashSet<String>,
}

impl<'a> Folder<'a> {
    pub fn new(names: &[&'a str]) -> Self {
        let names: Vec<Folded<'a>> = names.iter().map(|n| Folded::new(n)).collect();
        let mut exact = HashSet::new();
        let mut stem_spellings: HashMap<String, usize> = HashMap::new();
        let mut cbz = Vec::new();
        let mut sidecar_folds = HashSet::new();
        for name in &names {
            if let Some((raw, fold)) = ARCHIVE_EXTENSIONS.iter().find_map(|ext| name.strip(ext)) {
                if exact.insert(raw) {
                    *stem_spellings.entry(fold.to_string()).or_default() += 1;
                }
                if let Some((_, fold)) = name.strip(".cbz") {
                    cbz.push((name.raw, raw, fold.to_string()));
                }
            } else if let Some((raw, fold, _)) = name.mokuro() {
                sidecar_folds.insert(fold.to_string());
                if let Some((_, stem, _)) = split_base(raw, fold) {
                    sidecar_folds.insert(stem.to_string());
                }
            }
        }
        Folder {
            names,
            stem_spellings,
            cbz,
            sidecar_folds,
        }
    }

    /// The folded stems of the listed `.cbz` archives ([`volume_stems`]).
    pub fn volume_stems(&self) -> HashSet<String> {
        self.cbz.iter().map(|(_, _, fold)| fold.clone()).collect()
    }

    /// The listed files that belong to the archive `cbz_name`: its primary, cover,
    /// no-cover marker and layers, in listing order.
    ///
    /// Stricter than [`classify`], because what it returns may be deleted (R6):
    /// * a stem matches exactly, or by its fold only when it is the one archive here of
    ///   that folded stem (`vol 01.mokuro` is `Vol 01.cbz`'s alone; beside `Vol 01.cbz`
    ///   and `VOL 01.cbz`, a fold group, it is neither's);
    /// * a name whose base is any archive's stem, of any of the
    ///   [`ARCHIVE_EXTENSIONS`], is that archive's and is never a layer here
    ///   (`Vol 01.5.mokuro` beside `Vol 01.5.zip` classifies as layer `5`, but a sweep
    ///   leaves it).
    pub fn volume_files(&self, cbz_name: &str) -> Vec<(&'a str, VolumeFile<'a>)> {
        let anchor = Folded::new(cbz_name);
        let Some(anchor) = anchor.strip(".cbz") else {
            return Vec::new();
        };
        self.names
            .iter()
            .filter_map(|name| self.file_of(anchor, name).map(|file| (name.raw, file)))
            .collect()
    }

    /// What `name` is of the archive whose stem is `anchor` (`(raw, folded)`), when it is
    /// one of its files (R6: by its fold only when that archive is the one here of its
    /// folded stem).
    fn file_of(&self, anchor: (&str, &str), name: &Folded<'a>) -> Option<VolumeFile<'a>> {
        let (anchor_raw, anchor_fold) = anchor;
        let alone = self.stem_spellings.get(anchor_fold).is_none_or(|&n| n <= 1);
        let owns = |raw: &str, fold: &str| fold == anchor_fold && (raw == anchor_raw || alone);
        let (raw, fold, file) = if let Some((raw, fold, gz)) = name.mokuro() {
            (raw, fold, VolumeFile::Primary { gz })
        } else if let Some((raw, fold)) = name.strip(".webp") {
            (raw, fold, VolumeFile::Cover)
        } else if let Some((raw, fold)) = name.strip(".nocover") {
            (raw, fold, VolumeFile::NoCover)
        } else {
            return None;
        };
        if owns(raw, fold) {
            return Some(file);
        }
        let VolumeFile::Primary { gz } = file else {
            return None;
        };
        if self.stem_spellings.contains_key(fold) {
            return None;
        }
        let (raw_stem, fold_stem, id) = split_base(raw, fold)?;
        owns(raw_stem, fold_stem).then(|| VolumeFile::Layer {
            id: name.fold_part(id),
            gz,
        })
    }

    /// No `.mokuro` here can be `cbz_name`'s, in any spelling: checked without walking
    /// the listing.
    fn no_sidecars_for(&self, cbz_name: &str) -> bool {
        Folded::new(cbz_name)
            .strip(".cbz")
            .is_none_or(|(_, fold)| !self.sidecar_folds.contains(fold))
    }

    /// The primary sidecar of `cbz_name`: an exact stem before a folded one,
    /// `.mokuro` before `.mokuro.gz`.
    pub fn primary_of(&self, cbz_name: &str) -> Option<&'a str> {
        if self.no_sidecars_for(cbz_name) {
            return None;
        }
        let anchor = cbz_stem(cbz_name)?;
        let rank = |name: &str, gz: bool| {
            let exact = split_mokuro(name).is_some_and(|(base, _)| base == anchor);
            (!exact, gz)
        };
        self.volume_files(cbz_name)
            .into_iter()
            .filter_map(|(name, file)| match file {
                VolumeFile::Primary { gz } => Some((rank(name, gz), name)),
                _ => None,
            })
            .min()
            .map(|(_, name)| name)
    }

    /// `cbz_name`'s layer `id` (compared folded, R5) in any spelling of its stem,
    /// `.mokuro` before `.mokuro.gz`.
    pub fn layer_of(&self, cbz_name: &str, id: &str) -> Option<&'a str> {
        if self.no_sidecars_for(cbz_name) {
            return None;
        }
        let id = folded(id);
        self.volume_files(cbz_name)
            .into_iter()
            .filter_map(|(name, file)| match file {
                VolumeFile::Layer { id: found, gz } if found == id => Some((gz, name)),
                _ => None,
            })
            .min()
            .map(|(_, name)| name)
    }

    /// The listed `.cbz` archives whose folded stem is `fold_stem`, with their raw stems.
    fn cbz_of(&self, fold_stem: &str) -> Vec<(&'a str, &'a str)> {
        self.cbz
            .iter()
            .filter(|(_, _, fold)| fold == fold_stem)
            .map(|(name, raw, _)| (*name, *raw))
            .collect()
    }

    /// The stem a listed volume file `name` reads as, `(raw, folded)`: a `.mokuro`'s
    /// primary or layer stem (R3-R5), or a cover's or no-cover marker's base when it
    /// folds like a volume stem.
    fn owner_stem<'n>(&self, name: &'n Folded<'a>) -> Option<(&'a str, &'n str)> {
        let stems: HashSet<String> = self.volume_stems();
        if let Some((raw, fold, _, _)) = reading(name, &stems) {
            return Some((raw, fold));
        }
        [".webp", ".nocover"]
            .into_iter()
            .find_map(|suffix| name.strip(suffix))
            .filter(|(_, fold)| stems.contains(*fold))
    }

    /// The archive a volume file `name` (a `.mokuro`, layer, cover, no-cover marker) is
    /// read and shown with (R6): the listed `.cbz` spelled exactly like its stem, else
    /// the only one of that folded stem, else (a fold group) the code-point-smallest
    /// spelling. Bunko's own readers go by the store's case winner where a store names
    /// one ([`crate::fold_groups`]); this is the rule a client applies alone.
    pub fn display_archive(&self, name: &str) -> Option<&'a str> {
        let name = Folded::new(name);
        let (raw, fold) = self.owner_stem(&name)?;
        let found = self.cbz_of(fold);
        found
            .iter()
            .find(|(_, stem)| *stem == raw)
            .or_else(|| (found.len() == 1).then(|| &found[0]))
            .or_else(|| found.iter().min())
            .map(|(archive, _)| *archive)
    }

    /// The archive whose sweep (delete, rename) takes the volume file `name`, listed or
    /// not (R6): the first listed `.cbz` [`Folder::volume_files`] gives it to, exact
    /// spellings only in a fold group; `None` when no archive's sweep takes it.
    pub fn sweep_archive(&self, name: &str) -> Option<&'a str> {
        let name = Folded::new(name);
        self.cbz
            .iter()
            .find(|(_, raw, fold)| self.file_of((raw, fold), &name).is_some())
            .map(|(archive, _, _)| *archive)
    }
}

/// [`Folder::volume_files`] for a single archive.
pub fn volume_files<'a>(cbz_name: &str, names: &[&'a str]) -> Vec<(&'a str, VolumeFile<'a>)> {
    Folder::new(names).volume_files(cbz_name)
}

/// The names [`volume_files`] finds: everything that goes when `cbz_name` goes.
pub fn sweep_set(cbz_name: &str, names: &[&str]) -> Vec<String> {
    volume_files(cbz_name, names)
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

/// [`Folder::primary_of`] for a single archive.
pub fn primary_of<'a>(cbz_name: &str, names: &[&'a str]) -> Option<&'a str> {
    Folder::new(names).primary_of(cbz_name)
}

/// Is `id`, a dot-separated segment of a name, the id of a name nested under a volume
/// (`V.<id>.<rest>`)? A layer id with a letter once FOLDED (the heuristic of
/// `bunko_db::layer_sidecar_volume_path`, so `Vol 01.5.cbz` stays its own volume):
/// `V.EDIT.cbz` and `V.\u{212A}anji.cbz` (a Kelvin sign) still turn `V.edit.mokuro` and
/// `V.kanji.mokuro` into their primaries.
fn is_nested_id(id: &str) -> bool {
    let id = folded(id);
    is_layer_id(&id) && id.bytes().any(|b| b.is_ascii_lowercase())
}

/// Every stem `name` is nested under, `<stem>.<id>.<rest>` with a nested id
/// ([`is_nested_id`]), shortest first: `(raw, folded)`.
fn nested_stems<'n, 'a>(name: &'n Folded<'a>) -> Vec<(&'a str, &'n str)> {
    let fold: &'n str = &name.fold;
    let dots: Vec<usize> = fold.match_indices('.').map(|(i, _)| i).collect();
    let raw_dots: Vec<usize> = name.raw.match_indices('.').map(|(i, _)| i).collect();
    let mut out = Vec::new();
    for (k, &i) in dots.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let Some(&next) = dots.get(k + 1) else {
            break;
        };
        if next + 1 < fold.len() && is_nested_id(&fold[i + 1..next]) {
            out.push((&name.raw[..raw_dots[k]], &fold[..i]));
        }
    }
    out
}

/// The listed `.cbz` archives whose owner controls the file `name` of a folder listing
/// FILES `names` (series owners, 0.8). Empty when no listed archive governs it.
///
/// `name` belongs to the stem `s` when it is `s.cbz`; a sidecar `s.mokuro[.gz]`,
/// `s.webp`, `s.nocover`; a `.mokuro[.gz]` rule 2 reads as `s`'s primary or layer
/// ([`classify`], all-digit ids included); or any other name nested under it,
/// `s.<id>.<rest>`, `<id>` a layer id with a letter once folded ([`is_nested_id`]).
///
/// Of the listed archives it belongs to, the NEAREST (longest stem) governs it: a
/// listed `Berserk.v01.cbz` is its own uploader's volume, whoever uploads `Berserk.cbz`
/// later (OWN-1), and a name not listed yet takes the nearest listed volume above it
/// (creating `Berserk.v01.cbz` beside `Berserk.cbz` needs `Berserk.cbz`'s control).
/// Except that an archive hiding a reserved layer of the volume above it
/// (`V.curated.cbz`, `V.edit.cbz` beside `V.cbz`: its primary would be that layer)
/// passes control up to that volume, in either upload order: those are the names that
/// hijack `V`'s layers, and `V`'s arrival takes them back.
///
/// Stems match folded: an archive spelled exactly like the stem wins; otherwise every
/// listed spelling of that fold governs (a fold group), and all of them must be
/// controlled. Every step reads the folds alone, so `Q.ed\u{200B}it.edit.cbz` is governed
/// as `q.edit.edit.cbz` is.
pub fn governing_archives<'a>(name: &str, names: &[&'a str]) -> Vec<&'a str> {
    let folder = Folder::new(names);
    if folder.cbz.is_empty() {
        return Vec::new();
    }
    let name = Folded::new(name);
    let mut candidates: Vec<(&str, &str)> = Vec::new();
    if let Some(stem) = Suffix::ALL
        .into_iter()
        .find_map(|suffix| name.strip(suffix.as_str()))
    {
        candidates.push(stem);
    }
    // Rule 2's own reading of a `.mokuro` name (C7: `Vol 01.5.mokuro` is layer `5`).
    if let Some((raw, fold, _, _)) = reading(&name, &folder.volume_stems()) {
        candidates.push((raw, fold));
    }
    candidates.extend(nested_stems(&name));
    candidates.sort_by_key(|(_, fold)| std::cmp::Reverse(fold.len()));
    let Some((mut stem, mut found)) = candidates
        .into_iter()
        .map(|c| (c, folder.cbz_of(c.1)))
        .find(|(_, found)| !found.is_empty())
    else {
        return Vec::new();
    };
    while let Some(up) = hidden_layer_of(stem) {
        let above = folder.cbz_of(up.1);
        if above.is_empty() {
            break;
        }
        (stem, found) = (up, above);
    }
    match found.iter().find(|(_, raw)| *raw == stem.0) {
        Some((exact, _)) => vec![*exact],
        None => found.into_iter().map(|(n, _)| n).collect(),
    }
}

/// `s` when an archive stem (`(raw, folded)`) is `<s>.<id>` with `<id>` (folded) a layer
/// id of a reserved class: the archive whose primary would be `s`'s layer `<id>`.
fn hidden_layer_of<'r, 'f>(stem: (&'r str, &'f str)) -> Option<(&'r str, &'f str)> {
    let (fold, _) = hidden_layer_class(stem.1)?;
    let (raw, _) = stem.0.rsplit_once('.')?;
    Some((raw, fold))
}

/// [`hidden_layer_of`] of a folded stem, with the reserved class of the layer the
/// archive would hide.
fn hidden_layer_class(fold_stem: &str) -> Option<(&str, LayerClass)> {
    let (s, id) = fold_stem.rsplit_once('.')?;
    let class = layer_class(id);
    (!s.is_empty() && is_layer_id(id) && class != LayerClass::Other).then_some((s, class))
}

/// Would a NEW archive `name` hide a layer of a listed volume? `V.edit.cbz` beside
/// `V.cbz` turns `V.edit.mokuro`, an account's own correction of `V`, into the primary
/// of `V.edit.cbz` (R4), and `V.curated.cbz`, `V.curated-2.cbz` do the same to the
/// shared layer and its merge copies: true when `name` is `<s>.<id>.cbz` once folded,
/// `<id>` a layer id of a reserved class, and an archive of `<s>` is listed. Only
/// `.cbz` names define stems (R2).
pub fn hides_reserved_layer(name: &str, names: &[&str]) -> bool {
    let name = Folded::new(name);
    name.strip(".cbz")
        .and_then(|(_, fold)| hidden_layer_class(fold))
        .is_some_and(|(s, _)| volume_stems(names.iter().copied()).contains(s))
}

/// The reserved layer class (personal `edit`/`edit-<n>`, `curated`, `curated-<n>`) the
/// name `name` itself carries, its id folded (R5), in a folder whose listed FILES are
/// `names`; `None` for any other name: an `Other` layer, no layer at all, or a listed
/// archive's own primary (R4: `V.edit.mokuro` beside `V.edit.cbz`).
pub fn reserved_layer_of(name: &str, names: &[&str]) -> Option<LayerClass> {
    let (_, id, _) = split_layer(name)?;
    let class = layer_class(&id);
    if class == LayerClass::Other {
        return None;
    }
    match classify(name, &volume_stems(names.iter().copied())) {
        SidecarClass::Primary { .. } => None,
        _ => Some(class),
    }
}

// --- Deprecated: the either-fold machinery of rule v1, as thin rule-2 answers. ---------
//
// Rule v1 read stems by a second, narrower fold, so Bunko asked both and refused where
// they disagreed. With one fold nothing disagrees: each of these answers as rule 2 does,
// or empty, until its callers are gone.

/// Deprecated: rule v1's stem fold. Rule 2 reads every name by [`fold`].
#[deprecated(note = "rule 2 has one fold: use `fold`")]
pub fn stem_fold(name: &str) -> String {
    fold(name)
}

/// Deprecated: [`governing_archives`], sorted, each once. There is one fold to govern by.
#[deprecated(note = "rule 2 has one fold: use `governing_archives`")]
pub fn governing_archives_by_either_fold<'a>(name: &str, names: &[&'a str]) -> Vec<&'a str> {
    let mut out = governing_archives(name, names);
    out.sort_unstable();
    out.dedup();
    out
}

/// Deprecated: [`hides_reserved_layer`].
#[deprecated(note = "rule 2 has one fold: use `hides_reserved_layer`")]
pub fn hides_reserved_layer_by_either_fold(name: &str, names: &[&str]) -> bool {
    hides_reserved_layer(name, names)
}

/// Deprecated: no two folds part ways. What is left is the stateless hygiene refusal:
/// `[name]` when `name` hides a volume-file suffix ([`crate::names::hides_suffix`]:
/// `V.mo\u{212A}uro`, `V.cb\u{200B}z`) and no listed name of `names` is one entry with it
/// (a listed name is never refused); empty otherwise.
#[deprecated(note = "rule 2 has one fold: only `names::hides_suffix` is refused")]
pub fn fold_disputed(name: &str, names: &[&str]) -> Vec<String> {
    let target = folded(name);
    if crate::names::hides_suffix(name) && !names.iter().any(|n| folded(n) == target) {
        vec![name.to_string()]
    } else {
        Vec::new()
    }
}

/// Deprecated: always `None`. A name the fold makes a reserved layer IS that layer by
/// rule 2 ([`reserved_layer_of`]).
#[deprecated(note = "rule 2 reads ids folded: use `reserved_layer_of`")]
pub fn shadows_reserved_layer(_name: &str, _names: &[&str]) -> Option<LayerClass> {
    None
}

/// Deprecated: always `None`. An archive the fold makes `<s>.<reserved id>.cbz` hides
/// that layer by rule 2 ([`hides_reserved_layer`]).
#[deprecated(note = "rule 2 reads ids folded: use `hides_reserved_layer`")]
pub fn shadowing_archive(_name: &str) -> Option<LayerClass> {
    None
}

/// Deprecated: always empty. Two archives are one volume name exactly when they fold
/// alike (a fold group, [`crate::fold_groups`]).
#[deprecated(note = "rule 2 has one fold: volume names are ties only as fold groups")]
pub fn volume_name_ties<'a>(_name: &str, _names: &[&'a str]) -> Vec<&'a str> {
    Vec::new()
}

/// Deprecated: always empty ([`volume_name_ties`]).
#[deprecated(note = "rule 2 has one fold: volume names are ties only as fold groups")]
pub fn volume_name_tie_pairs<'a>(_names: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Hidden: every spelling of a name but the winner, and the files rule 2 gives to a
    /// shadowed archive (named by its exact stem), never the winner's own or a folder.
    #[test]
    fn a_shadowed_archives_own_files_are_hidden_too() {
        let none = |_: &str| None;
        let entries = [
            ("V.cbz", false),
            ("v.cbz", false),
            ("v.mokuro", false),
            ("v.webp", false),
            ("v.edit.mokuro", false),
            ("V.mokuro", false),
            ("W.cbz", false),
            ("w.webp", false),
            ("v.nocover", true),
        ];
        let mut hidden: Vec<String> = hidden_names(entries, none).into_iter().collect();
        hidden.sort();
        assert_eq!(hidden, ["v.cbz", "v.edit.mokuro", "v.mokuro", "v.webp"]);
        // Names one by the fold only through full case folding (`STRASSE`, `Straße`):
        // one group of archives, one of sidecars, each but its winner hidden.
        let entries = [
            ("STRASSE.cbz", false),
            ("Stra\u{df}e.cbz", false),
            ("strasse.mokuro", false),
            ("stra\u{df}e.mokuro", false),
        ];
        let mut hidden: Vec<String> = hidden_names(entries, none).into_iter().collect();
        hidden.sort();
        assert_eq!(hidden, ["Stra\u{df}e.cbz", "stra\u{df}e.mokuro"]);
        assert!(hidden_names([("a.cbz", false), ("a.mokuro", false)], none).is_empty());
    }

    #[test]
    fn case_losers_leave_one_spelling_per_fold() {
        let none = |_: &str| None;
        let names = [
            "Kingdom",
            "kingdom",
            "KINGDOM",
            "Other",
            "Pok\u{e9}mon",
            "POKE\u{301}MON",
        ];
        let losers = case_losers(names, none);
        let mut got: Vec<&str> = losers.iter().map(String::as_str).collect();
        got.sort();
        // The byte-smallest wins: `KINGDOM` and `POKÉMON` (decomposed) sort first.
        assert_eq!(got, ["Kingdom", "Pok\u{e9}mon", "kingdom"]);
        assert!(case_losers(["a", "b"], none).is_empty());
        // A preferred winner counts only when it is listed.
        let oldest = |f: &str| (f == "kingdom").then(|| "kingdom".to_string());
        let mut got: Vec<String> = case_losers(["Kingdom", "kingdom"], oldest)
            .into_iter()
            .collect();
        got.sort();
        assert_eq!(got, ["Kingdom"]);
        let gone = |_: &str| Some("KINGDOM".to_string());
        let got: Vec<String> = case_losers(["Kingdom", "kingdom"], gone)
            .into_iter()
            .collect();
        assert_eq!(got, ["kingdom"]);
    }

    fn vectors() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../interop/contract/sidecar-names.json");
        let text = std::fs::read_to_string(&path).expect("interop/contract/sidecar-names.json");
        serde_json::from_str(&text).expect("valid JSON")
    }

    fn strs(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect()
    }

    fn expected_class(e: &Value) -> SidecarClass<'_> {
        match e["kind"].as_str().unwrap() {
            "primary" => SidecarClass::Primary {
                stem: e["stem"].as_str().unwrap(),
                gz: e["gz"].as_bool().unwrap(),
            },
            "layer" => SidecarClass::Layer {
                stem: e["stem"].as_str().unwrap(),
                id: Cow::Borrowed(e["layer"].as_str().unwrap()),
                gz: e["gz"].as_bool().unwrap(),
            },
            "orphan" => SidecarClass::Orphan,
            other => panic!("unknown kind {other}"),
        }
    }

    /// A classification with its stem folded: what two spellings that decide alike share.
    fn folded_class(class: &SidecarClass<'_>) -> (u8, String, String, bool) {
        match class {
            SidecarClass::Primary { stem, gz } => (0, fold(stem), String::new(), *gz),
            SidecarClass::Layer { stem, id, gz } => (1, fold(stem), id.to_string(), *gz),
            SidecarClass::Orphan => (2, String::new(), String::new(), false),
        }
    }

    #[test]
    fn classify_vectors() {
        let v = vectors();
        assert_eq!(v["rule"], SIDECAR_RULE);
        let cases = v["classify"].as_array().unwrap();
        assert!(cases.len() >= 40);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let stems = volume_stems(strs(&case["folder"]));
            assert_eq!(
                classify(name, &stems),
                expected_class(&case["expect"]),
                "{name:?} in {:?}: {}",
                case["folder"],
                case["why"]
            );
        }
    }

    #[test]
    fn layer_class_vectors() {
        for case in vectors()["layer_class"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let want = match case["class"].as_str().unwrap() {
                "personal" => LayerClass::Personal,
                "curated" => LayerClass::Curated,
                "reserved" => LayerClass::Reserved,
                "other" => LayerClass::Other,
                other => panic!("unknown class {other}"),
            };
            assert_eq!(layer_class(id), want, "{id:?}");
        }
    }

    #[test]
    fn sweep_vectors() {
        for case in vectors()["sweep"].as_array().unwrap() {
            let archive = case["archive"].as_str().unwrap();
            let mut got = sweep_set(archive, &strs(&case["folder"]));
            got.sort();
            assert_eq!(got, strs(&case["expect"]), "{archive:?}: {}", case["why"]);
        }
    }

    /// R6: the archive a file is shown with, and the one whose sweep takes it.
    #[test]
    fn owner_vectors() {
        let cases = vectors();
        let cases = cases["owner"].as_array().unwrap();
        assert!(cases.len() >= 6);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let folder = Folder::new(&strs(&case["folder"]));
            assert_eq!(
                folder.display_archive(name),
                case["display"].as_str(),
                "display {name:?}: {}",
                case["why"]
            );
            assert_eq!(
                folder.sweep_archive(name),
                case["sweep"].as_str(),
                "sweep {name:?}: {}",
                case["why"]
            );
        }
    }

    /// A crafted listing classifies, name by name, as its canonical twin.
    #[test]
    fn equivalent_vectors() {
        let cases = vectors();
        let cases = cases["equivalent"].as_array().unwrap();
        assert!(cases.len() >= 6);
        for case in cases {
            let crafted = strs(&case["crafted"]);
            let canonical = strs(&case["canonical"]);
            assert_eq!(crafted.len(), canonical.len());
            let (a, b) = (
                volume_stems(crafted.clone()),
                volume_stems(canonical.clone()),
            );
            for (x, y) in crafted.iter().zip(&canonical) {
                assert_eq!(fold(x), fold(y), "{x:?} and {y:?} are one entry");
                assert_eq!(
                    folded_class(&classify(x, &a)),
                    folded_class(&classify(y, &b)),
                    "{x:?} as {y:?}: {}",
                    case["why"]
                );
            }
        }
    }

    #[test]
    fn layer_class_of_a_listed_name() {
        let stems = volume_stems(["Vol 1.cbz"]);
        assert_eq!(
            layer_class_of_name("Vol 1.edit-2.mokuro", &stems),
            Some(LayerClass::Personal)
        );
        assert_eq!(
            layer_class_of_name("Vol 1.curated.MOKURO", &stems),
            Some(LayerClass::Curated)
        );
        // Rule 2 (R5): ids are folded, so the id of `Vol 1.EDIT.mokuro` is `edit`.
        assert_eq!(
            layer_class_of_name("Vol 1.EDIT.mokuro", &stems),
            Some(LayerClass::Personal)
        );
        assert_eq!(
            layer_class_of_name("Vol 1.ed\u{200B}it.mokuro", &stems),
            Some(LayerClass::Personal)
        );
        assert_eq!(layer_class_of_name("Vol 1.mokuro", &stems), None);
        assert_eq!(layer_class_of_name("Vol 2.edit.mokuro", &stems), None);
    }

    #[test]
    fn primary_prefers_the_exact_stem_then_plain() {
        let names = ["V.cbz", "v.mokuro", "V.mokuro.gz"];
        assert_eq!(primary_of("V.cbz", &names), Some("V.mokuro.gz"));
        assert_eq!(
            primary_of("V.cbz", &["V.cbz", "V.MOKURO"]),
            Some("V.MOKURO")
        );
        assert_eq!(
            primary_of("V.cbz", &["V.cbz", "v.mokuro.gz", "v.MOKURO"]),
            Some("v.MOKURO")
        );
        assert_eq!(primary_of("V.cbz", &["V.cbz", "v.cbz", "v.mokuro"]), None);
        assert_eq!(primary_of("V.cbr", &["V.cbr", "V.mokuro"]), None);
        // Fold-only spellings (R1): one entry with the archive's on every host.
        assert_eq!(
            primary_of("Stra\u{df}e.cbz", &["Stra\u{df}e.cbz", "STRASSE.mokuro"]),
            Some("STRASSE.mokuro")
        );
        assert_eq!(
            primary_of("V.cbz", &["V.cbz", "V.mo\u{212A}uro"]),
            Some("V.mo\u{212A}uro")
        );
    }

    #[test]
    fn two_archives_of_one_folded_stem_claim_no_folded_sidecar() {
        let names = ["Vol 01.cbz", "VOL 01.cbz", "vol 01.mokuro"];
        assert_eq!(primary_of("Vol 01.cbz", &names), None);
        assert_eq!(primary_of("VOL 01.cbz", &names), None);
        // An exact stem is still its own.
        let names = ["Vol 01.cbz", "VOL 01.cbz", "VOL 01.mokuro"];
        assert_eq!(primary_of("VOL 01.cbz", &names), Some("VOL 01.mokuro"));
        assert_eq!(primary_of("Vol 01.cbz", &names), None);
    }

    #[test]
    fn a_layer_in_any_spelling_of_the_stem() {
        let folder = Folder::new(&["V.cbz", "v.gcv.mokuro.gz", "V.gcv.MOKURO", "V.5.cbz"]);
        assert_eq!(folder.layer_of("V.cbz", "gcv"), Some("V.gcv.MOKURO"));
        assert_eq!(folder.layer_of("V.cbz", "GCV"), Some("V.gcv.MOKURO"));
        assert_eq!(folder.layer_of("V.cbz", "hayai"), None);
        assert_eq!(folder.layer_of("W.cbz", "gcv"), None);
        assert_eq!(
            Folder::new(&["V.cbz", "v.gcv.mokuro.gz"]).layer_of("V.cbz", "gcv"),
            Some("v.gcv.mokuro.gz")
        );
        assert_eq!(folder.primary_of("V.cbz"), None);
        // R5: a miscased id is the layer.
        let folder = Folder::new(&["V.cbz", "V.GCV.mokuro"]);
        assert_eq!(folder.layer_of("V.cbz", "gcv"), Some("V.GCV.mokuro"));
    }

    #[test]
    fn the_outermost_listed_archive_governs_a_file() {
        let gov = governing_archives;
        let v = ["V.cbz", "W.cbz"];
        for name in [
            "V.cbz",
            "V.mokuro",
            "V.MOKURO.gz",
            "V.webp",
            "V.nocover",
            "V.edit.cbz",
            "V.hayai.webp",
            "V.curated.mokuro",
            "V.edit.curated.mokuro",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
        // Shortest stem wins, in either upload order; alone, a nested archive is its own.
        let both = ["V.cbz", "V.curated.cbz"];
        assert_eq!(gov("V.curated.cbz", &both), ["V.cbz"]);
        assert_eq!(gov("V.curated.mokuro", &both), ["V.cbz"]);
        assert_eq!(gov("V.curated.cbz", &["V.curated.cbz"]), ["V.curated.cbz"]);
        assert_eq!(
            gov("V.curated.mokuro", &["V.curated.cbz"]),
            ["V.curated.cbz"]
        );
        // Folded ids (C1): `V.EDIT` is `V.edit`.
        for name in [
            "V.EDIT.cbz",
            "V.Curated.cbz",
            "V.CURATED-2.cbz",
            "v.curated.cbz",
            "V.ed\u{200B}it.cbz",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
        // A decimal volume is its own; its sidecars too.
        let dec = ["Vol 01.cbz", "Vol 01.5.cbz"];
        assert_eq!(gov("Vol 01.5.cbz", &dec), ["Vol 01.5.cbz"]);
        assert_eq!(gov("Vol 01.5.mokuro", &dec), ["Vol 01.5.cbz"]);
        assert_eq!(gov("Vol 01.5.webp", &dec), ["Vol 01.5.cbz"]);
        // Without it, rule 2 reads `Vol 01.5.mokuro` as layer `5` of Vol 01 (C7) and
        // `V.2024.mokuro` as layer `2024` of V; other names need a letter in the id.
        assert_eq!(gov("Vol 01.5.mokuro", &["Vol 01.cbz"]), ["Vol 01.cbz"]);
        assert_eq!(gov("V.2024.mokuro", &v), ["V.cbz"]);
        assert!(gov("Vol 01.5.cbz", &["Vol 01.cbz"]).is_empty());
        assert!(gov("Vol 01.5.webp", &["Vol 01.cbz"]).is_empty());
        assert!(gov("Dr. Stone v1.cbz", &["Dr.cbz"]).is_empty());
        assert_eq!(
            gov("Dr. Stone v1.mokuro", &["Dr. Stone v1.cbz"]),
            ["Dr. Stone v1.cbz"]
        );
        // A new volume, an orphan, staging and dot names: nothing governs them.
        for name in [
            "X.cbz",
            "X.mokuro",
            "notes.txt",
            ".V.cbz.upload-1.tmp",
            ".V.cbz",
        ] {
            assert!(gov(name, &v).is_empty(), "{name}");
        }
        assert!(gov("V.mokuro", &[]).is_empty());
        // Only `.cbz` archives govern (R2).
        assert!(gov("V.mokuro", &["V.zip"]).is_empty());
        // Case variants: the exact spelling, else every one of them.
        let variants = ["Vol.cbz", "VOL.CBZ"];
        assert_eq!(gov("VOL.mokuro", &variants), ["VOL.CBZ"]);
        assert_eq!(gov("Vol.edit.cbz", &variants), ["Vol.cbz"]);
        let mut all = gov("vol.mokuro", &variants);
        all.sort();
        assert_eq!(all, ["VOL.CBZ", "Vol.cbz"]);
        // Ids fold: a Kelvin sign is a `k` (F1).
        for name in [
            "V.\u{212A}anji.cbz",
            "V.\u{212A}r.webp",
            "V.\u{212A}anji.mokuro",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
    }

    /// The nearest listed volume governs (OWN-1): `Berserk.cbz` arriving later takes
    /// nothing of another account's listed `Berserk.v01.cbz`; only the names hiding a
    /// reserved layer of a volume pass control up to it.
    #[test]
    fn the_nearest_listed_volume_governs_but_hijack_names_go_up() {
        let gov = governing_archives;
        let listed = [
            "Berserk.cbz",
            "Berserk.v01.cbz",
            "Berserk.Omake.cbz",
            "Berserk.curated.cbz",
            "Berserk.EDIT.cbz",
            "Berserk.pre-upgrade.cbz",
            "Berserk.previous-ocr.cbz",
            "Berserk.\u{212A}anji.cbz",
            "Berserk.edit.curated.cbz",
        ];
        for (name, want) in [
            ("Berserk.v01.cbz", "Berserk.v01.cbz"),
            ("Berserk.v01.mokuro", "Berserk.v01.cbz"),
            ("Berserk.v01.webp", "Berserk.v01.cbz"),
            ("Berserk.v01.curated.mokuro", "Berserk.v01.cbz"),
            ("Berserk.v01.x.cbz", "Berserk.v01.cbz"),
            ("Berserk.Omake.cbz", "Berserk.Omake.cbz"),
            ("Berserk.\u{212A}anji.cbz", "Berserk.\u{212A}anji.cbz"),
            ("Berserk.curated.cbz", "Berserk.cbz"),
            ("Berserk.curated.mokuro", "Berserk.cbz"),
            ("Berserk.EDIT.cbz", "Berserk.cbz"),
            // Hand-made layer ids like any other (neither id is reserved).
            ("Berserk.pre-upgrade.webp", "Berserk.pre-upgrade.cbz"),
            ("Berserk.previous-ocr.cbz", "Berserk.previous-ocr.cbz"),
            ("Berserk.edit.curated.cbz", "Berserk.cbz"),
            ("Berserk.mokuro", "Berserk.cbz"),
            ("Berserk.v02.cbz", "Berserk.cbz"),
        ] {
            assert_eq!(gov(name, &listed), [want], "{name}");
        }
        // Not listed yet: the nearest listed volume above it.
        assert_eq!(gov("Berserk.v01.cbz", &["Berserk.cbz"]), ["Berserk.cbz"]);
        assert_eq!(
            gov("Berserk.v01.x.mokuro", &["Berserk.cbz", "Berserk.v01.cbz"]),
            ["Berserk.v01.cbz"]
        );
        // A hijack name goes up only to a listed volume.
        assert_eq!(
            gov("V.curated.mokuro", &["V.curated.cbz"]),
            ["V.curated.cbz"]
        );
    }

    #[test]
    fn a_new_archive_may_hide_a_reserved_layer_only_of_no_volume() {
        let hides = hides_reserved_layer;
        let v = ["V.cbz"];
        for name in [
            "V.edit.cbz",
            "V.edit-2.cbz",
            "V.EDIT.cbz",
            "v.Curated.cbz",
            "V.curated-2.cbz",
            // Rule 2 reads the fold: one name with `V.edit.cbz` on every host.
            "V.ed\u{200B}it.cbz",
            "V.edit\u{AD}.cbz",
            "V.ed\u{131}t.cbz",
        ] {
            assert!(hides(name, &v), "{name}");
            assert!(!hides(name, &["W.cbz"]), "{name}");
        }
        assert!(hides("\u{17F}.curated.cbz", &["s.cbz"]));
        for name in [
            "V.hayai.cbz",
            "V.5.cbz",
            "V.edit.x.cbz",
            "V.edit.mokuro",
            "V.edit.zip",
            "V.cbz",
            "V.pre-upgrade.cbz",
            "V.previous-ocr.cbz",
        ] {
            assert!(!hides(name, &v), "{name}");
        }
        // Only `.cbz` archives define stems (R2).
        assert!(!hides("V.edit.cbz", &["V.zip"]));
    }

    /// The names only the fold joins with a volume's files are that volume's by rule 2:
    /// governed by it, and classified as its files.
    #[test]
    fn what_the_fold_joins_with_a_volume_is_the_volumes() {
        let gov = governing_archives;
        let listing = [
            "Stra\u{df}e.cbz",
            "Vol1.cbz",
            "\u{39f}\u{394}\u{39f}\u{3a3}.cbz",
            "i.cbz",
        ];
        for (name, archive) in [
            ("STRASSE.mokuro", "Stra\u{df}e.cbz"),
            ("strasse.webp", "Stra\u{df}e.cbz"),
            ("STRASSE.nocover", "Stra\u{df}e.cbz"),
            ("Stra\u{17F}\u{17F}e.mokuro.gz", "Stra\u{df}e.cbz"),
            ("STRASSE.hayai.mokuro", "Stra\u{df}e.cbz"),
            ("Vol\u{200B}1.mokuro", "Vol1.cbz"),
            ("Vol1\u{FE0F}.webp", "Vol1.cbz"),
            (
                "\u{3bf}\u{3b4}\u{3bf}\u{3c3}.mokuro",
                "\u{39f}\u{394}\u{39f}\u{3a3}.cbz",
            ),
            ("\u{131}.webp", "i.cbz"),
        ] {
            assert_eq!(gov(name, &listing), [archive], "{name}");
        }
        let folder = Folder::new(&["Stra\u{df}e.cbz", "STRASSE.mokuro", "strasse.edit.mokuro"]);
        assert_eq!(
            folder.volume_files("Stra\u{df}e.cbz"),
            [
                ("STRASSE.mokuro", VolumeFile::Primary { gz: false }),
                (
                    "strasse.edit.mokuro",
                    VolumeFile::Layer {
                        id: Cow::Borrowed("edit"),
                        gz: false
                    }
                ),
            ]
        );
    }

    /// The leak shapes of the either-fold era decide as their canonical twins.
    #[test]
    fn crafted_names_decide_as_their_fold() {
        // A reserved-layer hider: refused beside a listed `Q.edit.cbz`, and handed up to
        // its controller when listed first (OWN-2).
        let hider = "Q.ed\u{200B}it.edit.cbz";
        assert!(hides_reserved_layer(hider, &["Q.edit.cbz"]));
        assert!(hides_reserved_layer("q.edit.edit.cbz", &["Q.edit.cbz"]));
        assert_eq!(
            governing_archives(hider, &[hider, "Q.edit.cbz"]),
            ["Q.edit.cbz"]
        );
        for (crafted, base) in [
            ("V.ed\u{131}t.curated.cbz", "V.edit.cbz"),
            ("X\u{200B}.curated.cbz", "X.cbz"),
            ("STRASSE.curated.cbz", "Stra\u{df}e.cbz"),
        ] {
            assert!(hides_reserved_layer(crafted, &[base]), "{crafted:?}");
            assert_eq!(
                governing_archives(crafted, &[crafted, base]),
                [base],
                "{crafted:?}"
            );
        }
        // Nearest stem (OWN-1), exactly as `V.gcv.cbz` beside `V.cbz`.
        for (nested, base) in [
            ("Cafe\u{301}.gcv.cbz", "Caf\u{e9}.cbz"),
            (
                "\u{39f}\u{394}\u{39f}\u{3c2}.gcv.cbz",
                "\u{39f}\u{394}\u{39f}\u{3a3}.cbz",
            ),
        ] {
            let listing = [nested, base];
            assert_eq!(governing_archives(nested, &listing), [nested]);
            let mokuro = format!("{}.mokuro", cbz_stem(nested).unwrap());
            assert_eq!(governing_archives(&mokuro, &listing), [nested]);
            assert!(matches!(
                classify(&mokuro, &volume_stems(listing)),
                SidecarClass::Primary { .. }
            ));
        }
        // `X\u{200B}.cbz` beside `X.5.cbz`: `X.5.mokuro` is X.5's primary (R4).
        let listing = ["X\u{200B}.cbz", "X.5.cbz"];
        assert_eq!(
            classify("X.5.mokuro", &volume_stems(listing)),
            SidecarClass::Primary {
                stem: "X.5",
                gz: false
            }
        );
        assert_eq!(governing_archives("X.5.mokuro", &listing), ["X.5.cbz"]);
        // `STRASSE.mokuro` beside `Straße.cbz`: its primary.
        assert_eq!(
            classify("STRASSE.mokuro", &volume_stems(["Stra\u{df}e.cbz"])),
            SidecarClass::Primary {
                stem: "STRASSE",
                gz: false
            }
        );
        // `V.EDIT.mokuro` and `V.ed\u{200B}it.mokuro`: the personal layer.
        for name in [
            "V.EDIT.mokuro",
            "V.ed\u{200B}it.mokuro",
            "V.e\u{34F}dit.mokuro",
        ] {
            assert_eq!(
                reserved_layer_of(name, &["V.cbz"]),
                Some(LayerClass::Personal),
                "{name:?}"
            );
            assert_eq!(reserved_layer_of(name, &[]), Some(LayerClass::Personal));
        }
        assert_eq!(reserved_layer_of("V.edit.mokuro", &["V.edit.cbz"]), None);
        assert_eq!(reserved_layer_of("V.EDIT.mokuro", &["V.EDIT.cbz"]), None);
        assert_eq!(reserved_layer_of("V.EDIT.mokuro", &["v.edit.cbz"]), None);
        assert_eq!(
            reserved_layer_of("V.Curated.mokuro", &["V.cbz"]),
            Some(LayerClass::Curated)
        );
        assert_eq!(
            reserved_layer_of("V.curated-\u{200B}2.mokuro", &[]),
            Some(LayerClass::Reserved)
        );
        assert_eq!(reserved_layer_of("V.gcv.mokuro", &["V.cbz"]), None);
    }

    #[test]
    fn suffixes_decide_on_the_fold_and_cut_the_name_at_the_same_dot() {
        assert_eq!(strip_suffix_ci("é", ".cbz"), None);
        assert_eq!(strip_suffix_ci("aé.CBZ", ".cbz"), Some("aé"));
        assert_eq!(cbz_stem(".cbz"), Some(""));
        assert_eq!(cbz_stem("V.cb\u{200B}z"), Some("V"));
        assert_eq!(cbz_stem("V.cbz\u{200B}"), Some("V"));
        assert_eq!(cbz_stem("Stra\u{df}e.CBZ"), Some("Stra\u{df}e"));
        assert_eq!(cbz_stem("V.cbzz"), None);
        assert_eq!(split_mokuro("V.mo\u{212A}uro"), Some(("V", false)));
        assert_eq!(split_mokuro("V.MOKURO.G\u{200D}Z"), Some(("V", true)));
        assert_eq!(split_mokuro("V.mokuro.gz.gz"), None);
        assert_eq!(
            split_layer("V.ED\u{200B}IT.mokuro"),
            Some(("V", Cow::Borrowed("edit"), false))
        );
        assert_eq!(
            split_layer("Vol 1.Upper.mokuro.gz"),
            Some(("Vol 1", Cow::Borrowed("upper"), true))
        );
        assert_eq!(split_layer("Vol 1.Bad_Id.mokuro"), None);
        assert_eq!(split_layer("\u{200B}.edit.mokuro"), None);
        assert_eq!(strip_suffix_folded("V.Z\u{200B}IP", ".zip"), Some("V"));
        assert_eq!(strip_suffix_folded("V.zip", ".ZIP"), Some("V"));
        assert_eq!(suffix_of("V.cbz"), Some(Suffix::Cbz));
        assert_eq!(suffix_of("V.WE\u{200B}BP"), Some(Suffix::Webp));
        assert_eq!(suffix_of("V.Mokuro.GZ"), Some(Suffix::MokuroGz));
        assert_eq!(suffix_of("V.NoCover"), Some(Suffix::NoCover));
        assert_eq!(suffix_of("V.zip"), None);
    }

    /// The deprecated either-fold names answer as rule 2 does, or empty.
    #[test]
    #[allow(deprecated)]
    fn the_either_fold_names_are_rule_2_answers() {
        assert_eq!(stem_fold("Stra\u{df}e"), fold("STRASSE"));
        let listing = ["Stra\u{df}e.cbz", "Vol.cbz", "VOL.cbz"];
        for name in ["STRASSE.mokuro", "vol.webp", "X.mokuro", "Vol.edit.cbz"] {
            let mut want = governing_archives(name, &listing);
            want.sort_unstable();
            assert_eq!(governing_archives_by_either_fold(name, &listing), want);
        }
        assert!(hides_reserved_layer_by_either_fold(
            "V.ed\u{200B}it.cbz",
            &["V.cbz"]
        ));
        assert_eq!(
            fold_disputed("V.mo\u{212A}uro", &[]),
            ["V.mo\u{212A}uro".to_string()]
        );
        assert_eq!(
            fold_disputed("V.cb\u{200B}z", &["V.mokuro"]),
            ["V.cb\u{200B}z".to_string()]
        );
        // Listed in some spelling: never refused.
        assert!(fold_disputed("V.cb\u{200B}z", &["v.cb\u{200B}z"]).is_empty());
        for name in [
            "STRASSE.mokuro",
            "Vol\u{200B}1.webp",
            "V.EDIT.mokuro",
            "V.edit\u{200B}.mokuro",
            "X.mokuro",
        ] {
            assert!(fold_disputed(name, &["Stra\u{df}e.cbz", "Vol1.cbz", "V.cbz"]).is_empty());
        }
        assert_eq!(
            shadows_reserved_layer("X.ed\u{200B}it.mokuro", &["X.cbz"]),
            None
        );
        assert_eq!(shadowing_archive("X.ed\u{200B}it.cbz"), None);
        assert!(volume_name_ties("X\u{200B}.curated.cbz", &["X.cbz"]).is_empty());
        assert!(volume_name_tie_pairs(&["X\u{200B}.cbz", "X.5.cbz"]).is_empty());
    }
}
