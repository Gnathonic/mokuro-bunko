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
