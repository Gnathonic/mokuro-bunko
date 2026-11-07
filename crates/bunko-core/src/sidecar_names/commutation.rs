//! Rule 2 commutes with the fold: every public predicate gives the same answer on raw
//! names as on their folds (answers mapped back by the fold). Exhaustive over a small
//! alphabet of the spellings that ever tricked a second fold (zero width space and
//! joiner, dotless and dotted i, sharp s, long s, the sigmas, a decomposed accent, the
//! Kelvin sign, miscased and padded ids) and every listing of up to three names.

use std::collections::BTreeSet;

use super::*;

/// Pairs of stems one fold apart: each family is every tail on both.
const FAMILIES: [(&str, &str); 13] = [
    ("V", "v"),
    ("X", "X\u{200B}"),
    ("X", "x\u{200D}"),
    ("Q.edit", "Q.ed\u{200B}it"),
    ("\u{131}", "I"),
    ("\u{130}", "i\u{307}"),
    ("Stra\u{df}e", "STRASSE"),
    ("ss", "\u{17F}\u{17F}"),
    ("\u{3c3}", "\u{3c2}"),
    ("\u{3a3}", "\u{3c3}"),
    ("e\u{301}", "\u{e9}"),
    ("\u{212A}", "K"),
    ("Caf\u{e9}", "CAFE\u{301}"),
];

const TAILS: [&str; 12] = [
    ".cbz",
    ".mokuro",
    ".edit.mokuro",
    ".ED\u{200B}IT.mokuro",
    ".ed\u{131}t.cbz",
    ".curated.cbz",
    ".Cura\u{200D}ted.mokuro",
    ".gcv.mokuro",
    ".5.cbz",
    ".5.mokuro",
    ".webp",
    ".mo\u{212A}uro",
];

fn folds<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    names.into_iter().map(fold).collect()
}

fn class_key(class: &SidecarClass<'_>) -> (u8, String, String, bool) {
    match class {
        SidecarClass::Primary { stem, gz } => (0, fold(stem), String::new(), *gz),
        SidecarClass::Layer { stem, id, gz } => (1, fold(stem), id.to_string(), *gz),
        SidecarClass::Orphan => (2, String::new(), String::new(), false),
    }
}

fn files_key(files: &[(&str, VolumeFile<'_>)]) -> BTreeSet<(String, String)> {
    files
        .iter()
        .map(|(name, file)| (fold(name), format!("{file:?}")))
        .collect()
}

/// A listing, raw and folded, prepared once for every name asked against it.
struct Listing<'a> {
    raw: &'a [&'a str],
    folded: &'a [&'a str],
    stems: (HashSet<String>, HashSet<String>),
    folders: (Folder<'a>, Folder<'a>),
}

/// One name against one listing, raw and folded.
fn check_name(name: &str, listing: &Listing<'_>) {
    let Listing {
        raw,
        folded,
        stems,
        folders,
    } = listing;
    let f = fold(name);
    let at = || format!("{name:?} in {raw:?}");
    assert_eq!(
        class_key(&classify(name, &stems.0)),
        class_key(&classify(&f, &stems.1)),
        "classify {}",
        at()
    );
    assert_eq!(
        layer_class_of_name(name, &stems.0),
        layer_class_of_name(&f, &stems.1),
        "layer_class_of_name {}",
        at()
    );
    assert_eq!(
        folds(governing_archives(name, raw)),
        folds(governing_archives(&f, folded)),
        "governing_archives {}",
        at()
    );
    assert_eq!(
        hides_reserved_layer(name, raw),
        hides_reserved_layer(&f, folded),
        "hides_reserved_layer {}",
        at()
    );
    assert_eq!(
        reserved_layer_of(name, raw),
        reserved_layer_of(&f, folded),
        "reserved_layer_of {}",
        at()
    );
    assert_eq!(
        folders.0.display_archive(name).map(fold),
        folders.1.display_archive(&f).map(fold),
        "display_archive {}",
        at()
    );
}

/// The listing-free predicates, once per name.
fn check_alone(name: &str) {
    let f = fold(name);
    assert_eq!(suffix_of(name), suffix_of(&f), "suffix_of {name:?}");
    assert_eq!(
        split_layer(name).map(|(s, id, gz)| (fold(s), id.into_owned(), gz)),
        split_layer(&f).map(|(s, id, gz)| (fold(s), id.into_owned(), gz)),
        "split_layer {name:?}"
    );
    assert_eq!(
        split_mokuro(name).map(|(s, gz)| (fold(s), gz)),
        split_mokuro(&f).map(|(s, gz)| (fold(s), gz)),
        "split_mokuro {name:?}"
    );
    assert_eq!(
        cbz_stem(name).map(fold),
        cbz_stem(&f).map(fold),
        "cbz_stem {name:?}"
    );
    let segments: Vec<&str> = name.split('.').collect();
    let folded: Vec<&str> = f.split('.').collect();
    assert_eq!(
        segments.len(),
        folded.len(),
        "the fold keeps every dot: {name:?}"
    );
    for (s, t) in segments.iter().zip(&folded) {
        assert_eq!(is_nested_id(s), is_nested_id(t), "is_nested_id {s:?}");
        assert_eq!(layer_class(s), layer_class(t), "layer_class {s:?}");
    }
}

/// The per-archive answers of a listing.
fn check_folder(listing: &Listing<'_>) {
    let raw = listing.raw;
    let grouped = folds(raw.iter().copied()).len() < raw.len();
    let (a, b) = &listing.folders;
    for &archive in raw.iter().filter(|n| cbz_stem(n).is_some()) {
        let f = fold(archive);
        let at = || format!("{archive:?} in {raw:?}");
        let (x, y) = (
            files_key(&a.volume_files(archive)),
            files_key(&b.volume_files(&f)),
        );
        let (p, q) = (a.primary_of(archive).map(fold), b.primary_of(&f).map(fold));
        if grouped {
            // A fold group sweeps exact spellings only (R6): never more than its fold.
            assert!(x.is_subset(&y), "volume_files {}: {x:?} {y:?}", at());
            assert!(p.is_none() || p == q, "primary_of {}", at());
            continue;
        }
        assert_eq!(x, y, "volume_files {}", at());
        assert_eq!(p, q, "primary_of {}", at());
        for id in ["edit", "EDIT", "curated", "gcv", "5"] {
            assert_eq!(
                a.layer_of(archive, id).map(fold),
                b.layer_of(&f, id).map(fold),
                "layer_of {id} {}",
                at()
            );
        }
        for &name in raw {
            assert_eq!(
                a.sweep_archive(name).map(fold),
                b.sweep_archive(&fold(name)).map(fold),
                "sweep_archive {name:?} {}",
                at()
            );
        }
    }
}

/// Every listing of up to three names of one family; how many.
fn check_family(a: &str, b: &str) -> usize {
    let mut listings = 0;
    let pool: Vec<String> = [a, b]
        .iter()
        .flat_map(|stem| TAILS.iter().map(move |tail| format!("{stem}{tail}")))
        .collect();
    let pool: Vec<&str> = pool.iter().map(String::as_str).collect();
    let folded_pool: Vec<String> = pool.iter().map(|n| fold(n)).collect();
    for name in &pool {
        check_alone(name);
    }
    let n = pool.len();
    let mut picks: Vec<Vec<usize>> = vec![Vec::new()];
    for i in 0..n {
        picks.push(vec![i]);
        for j in i + 1..n {
            picks.push(vec![i, j]);
            for k in j + 1..n {
                picks.push(vec![i, j, k]);
            }
        }
    }
    for pick in picks {
        let raw: Vec<&str> = pick.iter().map(|&i| pool[i]).collect();
        let folded: Vec<&str> = pick.iter().map(|&i| folded_pool[i].as_str()).collect();
        let listing = Listing {
            raw: &raw,
            folded: &folded,
            stems: (
                volume_stems(raw.iter().copied()),
                volume_stems(folded.iter().copied()),
            ),
            folders: (Folder::new(&raw), Folder::new(&folded)),
        };
        for name in &pool {
            check_name(name, &listing);
        }
        check_folder(&listing);
        listings += 1;
    }
    listings
}

#[test]
fn every_rule_2_predicate_commutes_with_the_fold() {
    let listings: usize = std::thread::scope(|scope| {
        let families: Vec<_> = FAMILIES
            .iter()
            .map(|&(a, b)| scope.spawn(move || check_family(a, b)))
            .collect();
        families.into_iter().map(|f| f.join().unwrap()).sum()
    });
    assert_eq!(listings, FAMILIES.len() * (1 + 24 + 276 + 2024));
}
