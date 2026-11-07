//! `last_move_mode`: the rung a filesystem last needed, as doctor shows it. Its own test
//! binary (one process, one test): the cache is process-wide, and other tests' moves on
//! the same filesystem would race these assertions.

use bunko_store::durable::testing;
use bunko_store::fs::{MoveMode, identity, last_move_mode, move_noreplace};

#[test]
fn the_last_rung_a_filesystem_needed_is_remembered() {
    let d = tempfile::tempdir().unwrap();
    let dev = identity(d.path()).unwrap().dev;
    assert_eq!(last_move_mode(dev), None, "no move here yet");
    assert_eq!(last_move_mode(dev.wrapping_add(1)), None);
    let mut n = 0;
    let mut step = |mode: MoveMode, dir: bool| {
        let _g = testing::force(d.path(), mode);
        let (from, to) = (
            d.path().join(format!("a{n}")),
            d.path().join(format!("b{n}")),
        );
        n += 1;
        if dir {
            std::fs::create_dir(&from).unwrap();
        } else {
            std::fs::write(&from, b"x").unwrap();
        }
        assert_eq!(move_noreplace(&from, &to).unwrap(), mode);
        assert_eq!(last_move_mode(dev), Some(mode));
    };
    step(MoveMode::Copy, false);
    step(MoveMode::Link, false);
    step(MoveMode::Native, false);
    if cfg!(unix) {
        step(MoveMode::Claim, true);
    }
    step(MoveMode::Native, true);
    // A refused move remembers nothing new.
    std::fs::write(d.path().join("x"), b"x").unwrap();
    std::fs::write(d.path().join("y"), b"y").unwrap();
    let _g = testing::force(d.path(), MoveMode::Copy);
    assert!(move_noreplace(d.path().join("x"), d.path().join("y")).is_err());
    assert_eq!(last_move_mode(dev), Some(MoveMode::Native));
}
