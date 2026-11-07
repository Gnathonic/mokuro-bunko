//! The filename fold joins every pair of names the host the tests run on joins, for a
//! name that differs from another by one inserted code point: on Linux casefold that
//! is every default-ignorable code point the kernel drops (zero width space, joiners,
//! bidi marks, variation selectors, ...). On a case-sensitive disk the host joins none
//! of them and the test only checks the fold's side. Run the suite on a casefold
//! directory (`TMPDIR`) to compare with the kernel.

// The probe opens names on the host on purpose.
#![allow(clippy::disallowed_methods)]

use bunko_core::names::fold_name;

#[test]
fn the_fold_joins_every_name_this_host_joins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let mut joined = Vec::new();
    for cp in 0x80u32..=0x10FFFF {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        let name = format!("x{c}");
        if std::fs::symlink_metadata(dir.path().join(&name)).is_ok() {
            joined.push(cp);
            assert_eq!(
                fold_name(&name),
                fold_name("x"),
                "the host opens `x` for x + U+{cp:04X}; the fold must join them"
            );
        }
    }
    // Every code point the host drops is one the fold drops.
    for cp in joined {
        assert!(bunko_core::names::is_default_ignorable(
            char::from_u32(cp).unwrap()
        ));
    }
}

/// The superset probe of fold v2's table: for every code point the fold maps
/// (`M(c) != c`) and every one with a canonical decomposition, `x{c}` is created and
/// `x{M(c)}` / `x{NFD(c)}` looked up. Wherever the host opens one for the other (Linux
/// casefold joins full case folding and canonical equivalents), the fold must join them
/// too. A name the host refuses to create (a strict casefold directory and a code point
/// newer than its Unicode) is skipped.
#[test]
fn the_fold_joins_every_case_and_normalization_pair_this_host_joins() {
    use unicode_normalization::UnicodeNormalization;
    let dir = tempfile::tempdir().unwrap();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for &(cp, mapped) in bunko_core::fold_table::FOLD_MAP {
        let c = char::from_u32(cp).unwrap();
        pairs.push((format!("x{c}"), format!("x{mapped}")));
    }
    for c in (0x80u32..=0x10FFFF).filter_map(char::from_u32) {
        let nfd: String = c.to_string().nfd().collect();
        if nfd != c.to_string() {
            pairs.push((format!("x{c}"), format!("x{nfd}")));
        }
    }
    let mut joined = 0;
    for (created, probe) in &pairs {
        let path = dir.path().join(created);
        if std::fs::write(&path, b"x").is_err() {
            continue;
        }
        if std::fs::symlink_metadata(dir.path().join(probe)).is_ok() {
            joined += 1;
            assert_eq!(
                fold_name(created),
                fold_name(probe),
                "the host opens {created:?} for {probe:?}; the fold must join them"
            );
        }
        std::fs::remove_file(&path).unwrap();
    }
    eprintln!("{joined} of {} pairs joined by this host", pairs.len());
}
