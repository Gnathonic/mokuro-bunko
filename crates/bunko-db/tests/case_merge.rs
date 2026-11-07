//! The local case merge's journal (`bunko_db::case_merge`): groups and steps journaled
//! whole, a step's rows and its done state committed as one, every `Carry` against
//! fixture rows, blocks, owner snapshots and deferred adoptions.

use bunko_db::case_merge::{
    Carry, GroupState, NewBlock, NewGroup, NewStep, Stat, StepKind, StepNote, StepState, Tally,
};
use bunko_db::{Database, DbError, DbOptions, OcrSidecar};
use serde_json::{Value, json};

fn temp_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(
        dir.path().join("mokuro.db"),
        &DbOptions {
            bcrypt_cost: 4,
            ..DbOptions::default()
        },
    )
    .unwrap();
    (dir, db)
}

fn group(id: &str, scope: &str, created_at: i64) -> NewGroup {
    NewGroup {
        id: id.into(),
        scope: scope.into(),
        parent: String::new(),
        winner: "Kingdom".into(),
        losers: vec!["kingdom".into()],
        origin: "auto".into(),
        fingerprint: format!("fp-{id}"),
        created_at,
    }
}

fn mv(src: &str, dest: &str) -> NewStep {
    NewStep {
        vg: "Vol 1".into(),
        src: Some(src.into()),
        dest: Some(dest.into()),
        src_stat: Some(Stat {
            dev: 66,
            ino: 1234,
            size: 10,
            mtime_ns: 1_700_000_000_123_456_789,
        }),
        ..NewStep::new(StepKind::Move)
    }
}

fn ocr(path: &str, volume: &str, generation: &str) -> OcrSidecar {
    OcrSidecar {
        sidecar_path: path.into(),
        volume_key: volume.into(),
        generation_id: generation.into(),
        generation_name: "Hayai".into(),
        machine: "local".into(),
        ..OcrSidecar::default()
    }
}

fn count(db: &Database, sql: &str) -> i64 {
    db.with_writer_connection(|c| c.query_row(sql, [], |r| r.get(0)).unwrap())
}

#[test]
fn a_group_and_all_its_steps_are_journaled_as_one() {
    let (_d, db) = temp_db();
    let mut big = mv("kingdom/Vol 2.cbz", "Kingdom/Vol 2.cbz");
    // Identities are 64-bit unsigned; SQLite keeps their bit pattern.
    big.src_stat = Some(Stat {
        dev: u64::MAX - 1,
        ino: (1 << 63) + 5,
        size: 1 << 40,
        mtime_ns: -1_000,
    });
    big.is_dir = true;
    big.extra = Some(r#"{"step":"move","vg":"Vol 2"}"#.into());
    big.sha256 = Some("ab".repeat(32));
    let steps = vec![
        mv("kingdom/Vol 1.cbz", "Kingdom/Vol 1.cbz"),
        big,
        NewStep {
            phase: 4,
            src: Some("kingdom".into()),
            ..NewStep::new(StepKind::Finish)
        },
        NewStep {
            phase: 5,
            ..NewStep::new(StepKind::OverlayFold)
        },
        NewStep {
            phase: 6,
            ..NewStep::new(StepKind::Close)
        },
    ];
    db.open_case_merge_group(&group("g1", "{\"s\":1}", 1000), &steps)
        .unwrap();
    let g = db.case_merge_group("g1").unwrap().unwrap();
    assert_eq!(g.state, GroupState::Open);
    assert_eq!(g.losers, ["kingdom"]);
    assert_eq!(g.fingerprint, "fp-g1");
    assert_eq!((g.moved, g.kept, g.blocked, g.audited), (0, 0, 0, false));
    let got = db.case_merge_steps("g1").unwrap();
    assert_eq!(got.len(), 5);
    for (i, (s, want)) in got.iter().zip(&steps).enumerate() {
        assert_eq!(s.seq, i as i64);
        assert_eq!(s.state, StepState::Planned);
        assert_eq!(
            (s.kind, s.phase, &s.src, &s.dest, s.is_dir),
            (want.kind, want.phase, &want.src, &want.dest, want.is_dir)
        );
        assert_eq!(s.src_stat, want.src_stat);
        assert_eq!((&s.sha256, &s.extra), (&want.sha256, &want.extra));
        assert_eq!(s.dest_id, None);
    }
    assert_eq!(db.case_merge_step("g1", 1).unwrap().unwrap(), got[1]);
    assert!(db.case_merge_step("g1", 9).unwrap().is_none());

    // The same id again is refused and adds nothing.
    let e = db
        .open_case_merge_group(&group("g1", "{}", 2000), &steps[..1])
        .unwrap_err();
    assert!(matches!(e, DbError::Conflict(_)), "{e}");
    assert_eq!(db.case_merge_steps("g1").unwrap().len(), 5);
    assert!(matches!(
        db.open_case_merge_group(&group("", "{}", 1), &[]),
        Err(DbError::Invalid(_))
    ));
}

/// A step that cannot be written leaves no group: a pass sees the whole plan or none.
#[test]
fn a_failed_journal_write_leaves_no_group() {
    let (_d, db) = temp_db();
    db.with_writer_connection(|c| {
        c.execute_batch(
            "CREATE TRIGGER no_third BEFORE INSERT ON case_merge_steps WHEN NEW.seq = 2 \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap()
    });
    let steps = vec![mv("a/1.cbz", "A/1.cbz"); 3];
    assert!(
        db.open_case_merge_group(&group("g", "{}", 1), &steps)
            .is_err()
    );
    assert!(db.case_merge_group("g").unwrap().is_none());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM case_merge_steps"), 0);
}

#[test]
fn steps_move_through_their_states_and_done_is_final() {
    let (_d, db) = temp_db();
    db.open_case_merge_group(&group("g", "{}", 1), &[mv("a/1.cbz", "A/1.cbz")])
        .unwrap();
    assert!(
        db.set_case_merge_step_state(
            "g",
            0,
            StepState::Copying,
            &StepNote {
                mode: Some("copy".into()),
                keep: Some("case-merge/g/library/a/1.cbz".into()),
                ..StepNote::default()
            },
        )
        .unwrap()
    );
    assert!(
        db.set_case_merge_step_state(
            "g",
            0,
            StepState::Copied,
            &StepNote {
                dest_id: Some((u64::MAX, 7)),
                sha256: Some("cd".repeat(32)),
                ..StepNote::default()
            },
        )
        .unwrap()
    );
    let s = db.case_merge_step("g", 0).unwrap().unwrap();
    assert_eq!(s.state, StepState::Copied);
    // A note leaves what it does not name.
    assert_eq!(s.mode.as_deref(), Some("copy"));
    assert_eq!(s.keep.as_deref(), Some("case-merge/g/library/a/1.cbz"));
    assert_eq!(s.dest_id, Some((u64::MAX, 7)));
    assert!(matches!(
        db.set_case_merge_step_state("g", 0, StepState::Done, &StepNote::default()),
        Err(DbError::Invalid(_))
    ));
    db.commit_case_merge_step("g", 0, &[], Tally::default(), &StepNote::default())
        .unwrap()
        .unwrap();
    // Done stays done.
    assert!(
        !db.set_case_merge_step_state("g", 0, StepState::Planned, &StepNote::default())
            .unwrap()
    );
    assert_eq!(
        db.case_merge_step("g", 0).unwrap().unwrap().state,
        StepState::Done
    );
    assert!(
        !db.set_case_merge_step_state("g", 5, StepState::Skipped, &StepNote::default())
            .unwrap()
    );
}

#[test]
fn a_commit_applies_its_carries_once_with_the_done_state_and_tally() {
    let (_d, db) = temp_db();
    db.record_volume_upload("kingdom/Vol 1.cbz", "alice")
        .unwrap();
    db.remember_volume_uuid("kingdom/Vol 1.cbz", "u-1").unwrap();
    db.open_case_merge_group(
        &group("g", "{}", 1),
        &[mv("kingdom/Vol 1.cbz", "Kingdom/Vol 1.cbz")],
    )
    .unwrap();
    let carries = [
        Carry::RenameUpload {
            old: "kingdom/Vol 1.cbz".into(),
            new: "Kingdom/Vol 1.cbz".into(),
        },
        Carry::RenameUuid {
            old: "kingdom/Vol 1.cbz".into(),
            new: "Kingdom/Vol 1.cbz".into(),
        },
    ];
    let note = StepNote {
        mode: Some("native".into()),
        dest_id: Some((66, 1234)),
        ..StepNote::default()
    };
    let tally = Tally {
        moved: 1,
        ..Tally::default()
    };
    let out = db
        .commit_case_merge_step("g", 0, &carries, tally, &note)
        .unwrap()
        .unwrap();
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|o| o.changed == 1 && o.dropped.is_empty()));
    assert_eq!(
        db.get_volume_owner_exact("Kingdom/Vol 1.cbz")
            .unwrap()
            .as_deref(),
        Some("alice")
    );
    assert_eq!(
        db.remembered_volume_uuid("Kingdom/Vol 1.cbz")
            .unwrap()
            .as_deref(),
        Some("u-1")
    );
    let s = db.case_merge_step("g", 0).unwrap().unwrap();
    assert_eq!(s.state, StepState::Done);
    assert_eq!(s.dest_id, Some((66, 1234)));
    assert_eq!(s.mode.as_deref(), Some("native"));
    assert_eq!(db.case_merge_group("g").unwrap().unwrap().moved, 1);

    // Again (a resume that did not see the commit): nothing is applied twice.
    db.record_volume_upload("kingdom/Vol 1.cbz", "mallory")
        .unwrap();
    assert!(
        db.commit_case_merge_step("g", 0, &carries, tally, &note)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.get_volume_owner_exact("kingdom/Vol 1.cbz")
            .unwrap()
            .as_deref(),
        Some("mallory")
    );
    assert_eq!(
        db.get_volume_owner_exact("Kingdom/Vol 1.cbz")
            .unwrap()
            .as_deref(),
        Some("alice")
    );
    assert_eq!(db.case_merge_group("g").unwrap().unwrap().moved, 1);
    assert!(matches!(
        db.commit_case_merge_step("g", 3, &[], tally, &note),
        Err(DbError::Invalid(_))
    ));
}

/// A failure after the first carry rolls back everything: the first carry, the done
/// state, the tally.
#[test]
fn a_commit_that_fails_part_way_applies_nothing() {
    let (_d, db) = temp_db();
    db.record_volume_upload("kingdom/Vol 1.cbz", "alice")
        .unwrap();
    db.remember_volume_uuid("kingdom/Vol 1.cbz", "u-1").unwrap();
    db.open_case_merge_group(
        &group("g", "{}", 1),
        &[mv("kingdom/Vol 1.cbz", "Kingdom/Vol 1.cbz")],
    )
    .unwrap();
    db.with_writer_connection(|c| {
        c.execute_batch(
            "CREATE TRIGGER fail_uuid BEFORE DELETE ON volume_identities \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap()
    });
    let carries = [
        Carry::RenameUpload {
            old: "kingdom/Vol 1.cbz".into(),
            new: "Kingdom/Vol 1.cbz".into(),
        },
        Carry::ForgetUuid {
            path: "kingdom/Vol 1.cbz".into(),
        },
    ];
    let tally = Tally {
        moved: 1,
        kept: 1,
        blocked: 1,
    };
    let e = db
        .commit_case_merge_step("g", 0, &carries, tally, &StepNote::default())
        .unwrap_err();
    assert!(e.to_string().contains("injected"), "{e}");
    assert_eq!(
        db.get_volume_owner_exact("kingdom/Vol 1.cbz")
            .unwrap()
            .as_deref(),
        Some("alice"),
        "the first carry was rolled back"
    );
    assert!(
        db.get_volume_owner_exact("Kingdom/Vol 1.cbz")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.case_merge_step("g", 0).unwrap().unwrap().state,
        StepState::Planned
    );
    let g = db.case_merge_group("g").unwrap().unwrap();
    assert_eq!((g.moved, g.kept, g.blocked), (0, 0, 0));
    // apply_carries is one transaction too.
    assert!(db.apply_carries(&carries).is_err());
    assert!(
        db.get_volume_owner_exact("Kingdom/Vol 1.cbz")
            .unwrap()
            .is_none()
    );
}

#[test]
fn uploads_rename_carry_if_absent_and_forget() {
    let (_d, db) = temp_db();
    db.record_volume_upload("kingdom/Vol 1.cbz", "alice")
        .unwrap();
    db.record_volume_upload("Kingdom/Vol 1.cbz", "bob").unwrap();
    db.record_volume_upload("kingdom/Vol 2.cbz", "carol")
        .unwrap();
    db.record_volume_upload("kingdom/Vol 3.cbz", "dave")
        .unwrap();
    db.record_volume_upload("KINGDOM/Vol 3.cbz", "erin")
        .unwrap();
    let out = db
        .apply_carries(&[
            // The winner has a row: it stays, the loser's is dropped and reported.
            Carry::CarryUploadIfAbsent {
                from: "kingdom/Vol 1.cbz".into(),
                to: "Kingdom/Vol 1.cbz".into(),
            },
            // The winner has none: the loser's row moves to the winner's key. A sidecar
            // path names its archive.
            Carry::CarryUploadIfAbsent {
                from: "kingdom/Vol 2.mokuro".into(),
                to: "Kingdom/Vol 2.cbz".into(),
            },
            // No loser row: nothing.
            Carry::CarryUploadIfAbsent {
                from: "kingdom/Vol 9.cbz".into(),
                to: "Kingdom/Vol 9.cbz".into(),
            },
            // A rename over a row reports the row it replaced.
            Carry::RenameUpload {
                old: "kingdom/Vol 3.cbz".into(),
                new: "KINGDOM/Vol 3.cbz".into(),
            },
        ])
        .unwrap();
    let owner = |p: &str| db.get_volume_owner_exact(p).unwrap();
    assert_eq!(owner("Kingdom/Vol 1.cbz").as_deref(), Some("bob"));
    assert_eq!(owner("kingdom/Vol 1.cbz"), None);
    assert_eq!(out[0].changed, 1);
    assert_eq!(out[0].dropped.len(), 1);
    assert_eq!(out[0].dropped[0]["table"], "volume_uploads");
    assert_eq!(out[0].dropped[0]["volume_key"], "kingdom/Vol 1.cbz");
    assert_eq!(out[0].dropped[0]["uploader_username"], "alice");
    assert_eq!(owner("Kingdom/Vol 2.cbz").as_deref(), Some("carol"));
    assert_eq!(owner("kingdom/Vol 2.cbz"), None);
    assert!(out[1].dropped.is_empty() && out[1].changed == 1);
    assert_eq!((out[2].changed, out[2].dropped.len()), (0, 0));
    assert_eq!(owner("KINGDOM/Vol 3.cbz").as_deref(), Some("dave"));
    assert_eq!(out[3].dropped[0]["uploader_username"], "erin");
    // Forget is exact: a row of another spelling stays.
    let out = db
        .apply_carries(&[Carry::ForgetUpload {
            path: "Kingdom/Vol 1.cbz".into(),
        }])
        .unwrap();
    assert_eq!(out[0].dropped[0]["uploader_username"], "bob");
    assert_eq!(owner("Kingdom/Vol 1.cbz"), None);
    assert_eq!(owner("Kingdom/Vol 2.cbz").as_deref(), Some("carol"));
    // A rename with no row at the old key leaves the new key's row alone.
    let out = db
        .apply_carries(&[Carry::RenameUpload {
            old: "nowhere/Vol 2.cbz".into(),
            new: "Kingdom/Vol 2.cbz".into(),
        }])
        .unwrap();
    assert_eq!((out[0].changed, out[0].dropped.len()), (0, 0));
    assert_eq!(owner("Kingdom/Vol 2.cbz").as_deref(), Some("carol"));
}

#[test]
fn ocr_rows_follow_their_files_and_report_what_they_replace() {
    let (_d, db) = temp_db();
    for (p, v, g) in [
        ("kingdom/Vol 1.mokuro", "kingdom/Vol 1.cbz", "g1"),
        ("kingdom/Vol 1.hayai.mokuro", "kingdom/Vol 1.cbz", "g2"),
        ("Kingdom/Vol 1.hayai.mokuro", "Kingdom/Vol 1.cbz", "old"),
        ("kingdom/VOL 2.edit.mokuro", "kingdom/VOL 2.cbz", "g3"),
        ("kingdom/Vol 3.mokuro", "kingdom/Vol 3.cbz", "g4"),
        ("kingdom/Vol 3.gcv.mokuro", "kingdom/Vol 3.cbz", "g5"),
        ("kingdom/Vol 4.webp", "kingdom/Vol 4.cbz", "g6"),
    ] {
        db.record_ocr_sidecar(&ocr(p, v, g)).unwrap();
    }
    let out = db
        .apply_carries(&[
            Carry::RenameOcrVolume {
                old: "kingdom/Vol 1.cbz".into(),
                new: "Kingdom/Vol 1.cbz".into(),
            },
            Carry::RenameOcrSidecar {
                old: "kingdom/VOL 2.edit.mokuro".into(),
                new: "Kingdom/Vol 2.edit-2.mokuro".into(),
                volume: "Kingdom/Vol 2.cbz".into(),
            },
            Carry::ForgetOcrVolume {
                volume: "kingdom/Vol 3.cbz".into(),
            },
            Carry::ForgetOcrSidecar {
                path: "/kingdom/Vol 4.webp/".into(),
            },
            Carry::ForgetOcrSidecar {
                path: "kingdom/none.mokuro".into(),
            },
        ])
        .unwrap();
    let row = |p: &str| db.get_ocr_sidecar(p).unwrap();
    assert_eq!(out[0].changed, 2);
    assert_eq!(row("Kingdom/Vol 1.mokuro").unwrap().generation_id, "g1");
    let layer = row("Kingdom/Vol 1.hayai.mokuro").unwrap();
    assert_eq!(
        (layer.generation_id.as_str(), layer.volume_key.as_str()),
        ("g2", "Kingdom/Vol 1.cbz")
    );
    assert_eq!(out[0].dropped.len(), 1, "the winner's replaced row");
    assert_eq!(out[0].dropped[0]["generation_id"], "old");
    assert!(row("kingdom/Vol 1.mokuro").is_none());
    let moved = row("Kingdom/Vol 2.edit-2.mokuro").unwrap();
    assert_eq!(
        (moved.generation_id.as_str(), moved.volume_key.as_str()),
        ("g3", "Kingdom/Vol 2.cbz")
    );
    assert_eq!((out[1].changed, out[1].dropped.len()), (1, 0));
    assert_eq!(out[2].changed, 2);
    let gens: Vec<&Value> = out[2].dropped.iter().map(|r| &r["generation_id"]).collect();
    assert_eq!(gens, [&json!("g5"), &json!("g4")]);
    assert!(row("kingdom/Vol 3.mokuro").is_none());
    assert_eq!(out[3].dropped[0]["sidecar_path"], "kingdom/Vol 4.webp");
    assert_eq!((out[4].changed, out[4].dropped.len()), (0, 0));
    assert_eq!(db.list_ocr_sidecars().unwrap().len(), 3);
}

#[test]
fn uuids_rename_carry_if_absent_and_forget() {
    let (_d, db) = temp_db();
    db.remember_volume_uuid("kingdom/Vol 1.cbz", "u-loser-1")
        .unwrap();
    db.remember_volume_uuid("Kingdom/Vol 1.cbz", "u-winner-1")
        .unwrap();
    db.remember_volume_uuid("kingdom/Vol 2.cbz", "u-loser-2")
        .unwrap();
    db.remember_volume_uuid("kingdom/Vol 3.cbz", "u-3").unwrap();
    db.remember_volume_uuid("Kingdom/Vol 3.cbz", "u-3-winner")
        .unwrap();
    let out = db
        .apply_carries(&[
            Carry::CarryUuidIfAbsent {
                from: "kingdom/Vol 1.cbz".into(),
                to: "Kingdom/Vol 1.cbz".into(),
            },
            Carry::CarryUuidIfAbsent {
                from: "kingdom/Vol 2.cbz".into(),
                to: "Kingdom/Vol 2.cbz".into(),
            },
            Carry::RenameUuid {
                old: "kingdom/Vol 3.cbz".into(),
                new: "Kingdom/Vol 3.cbz".into(),
            },
            Carry::ForgetUuid {
                path: "Kingdom/Vol 1.mokuro".into(),
            },
        ])
        .unwrap();
    let uuid = |p: &str| db.remembered_volume_uuid(p).unwrap();
    assert_eq!(out[0].dropped[0]["volume_uuid"], "u-loser-1");
    assert_eq!(uuid("kingdom/Vol 1.cbz"), None);
    assert_eq!(uuid("Kingdom/Vol 2.cbz").as_deref(), Some("u-loser-2"));
    assert!(out[1].dropped.is_empty());
    assert_eq!(uuid("Kingdom/Vol 3.cbz").as_deref(), Some("u-3"));
    assert_eq!(out[2].dropped[0]["volume_uuid"], "u-3-winner");
    assert_eq!(out[3].dropped[0]["volume_uuid"], "u-winner-1");
    assert_eq!(uuid("Kingdom/Vol 1.cbz"), None);
}

#[test]
fn the_entry_cache_row_is_forgotten_unreported() {
    let (_d, db) = temp_db();
    let entry = serde_json::Map::new();
    db.put_cached_volume_entry("kingdom/Vol 1.cbz", "kingdom", &entry, 1, 1.0, "")
        .unwrap();
    db.put_cached_volume_entry("Kingdom/Vol 1.cbz", "kingdom", &entry, 1, 1.0, "")
        .unwrap();
    let out = db
        .apply_carries(&[Carry::ForgetEntryCache {
            volume: "kingdom/Vol 1.cbz".into(),
        }])
        .unwrap();
    assert_eq!((out[0].changed, out[0].dropped.len()), (1, 0));
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM series_entry_cache"),
        1,
        "only that key"
    );
}

#[test]
fn groups_pause_resume_close_and_count_recreations() {
    let (_d, db) = temp_db();
    for (id, scope, at) in [("a", "S1", 100), ("b", "S1", 200), ("c", "S2", 300)] {
        db.open_case_merge_group(&group(id, scope, at), &[mv("x", "y")])
            .unwrap();
    }
    assert!(db.pause_case_merge_group("a", "OCR is busy").unwrap());
    let a = db.case_merge_group("a").unwrap().unwrap();
    assert_eq!(a.state, GroupState::Paused);
    assert_eq!(a.pause_reason.as_deref(), Some("OCR is busy"));
    let ids = |gs: Vec<bunko_db::case_merge::Group>| -> Vec<String> {
        gs.into_iter().map(|g| g.id).collect()
    };
    assert_eq!(
        ids(db.unfinished_case_merge_groups().unwrap()),
        ["a", "b", "c"]
    );
    assert!(db.resume_case_merge_group("a").unwrap());
    assert!(!db.resume_case_merge_group("a").unwrap(), "not paused");
    assert_eq!(
        db.case_merge_group("a").unwrap().unwrap().pause_reason,
        None
    );

    // A group that changed something counts toward the recreate limit once done.
    db.commit_case_merge_step(
        "a",
        0,
        &[],
        Tally {
            moved: 1,
            ..Tally::default()
        },
        &StepNote::default(),
    )
    .unwrap();
    assert_eq!(db.case_merge_recreations("S1", "auto", 0).unwrap(), 0);
    assert!(db.close_case_merge_group("a", 150).unwrap());
    assert!(
        !db.close_case_merge_group("a", 160).unwrap(),
        "already done"
    );
    assert!(!db.pause_case_merge_group("a", "late").unwrap());
    let a = db.case_merge_group("a").unwrap().unwrap();
    assert_eq!((a.state, a.finished_at), (GroupState::Done, Some(150)));
    // `b` closes having changed nothing: not a recreation.
    assert!(db.close_case_merge_group("b", 250).unwrap());
    assert_eq!(db.case_merge_recreations("S1", "auto", 0).unwrap(), 1);
    assert_eq!(db.case_merge_recreations("S1", "auto", 101).unwrap(), 0);
    assert_eq!(db.case_merge_recreations("S1", "admin", 0).unwrap(), 0);
    assert_eq!(db.case_merge_recreations("S2", "auto", 0).unwrap(), 0);
    assert_eq!(ids(db.unfinished_case_merge_groups().unwrap()), ["c"]);
    assert_eq!(
        ids(db.recent_case_merge_groups(2).unwrap()),
        ["c", "b"],
        "newest first"
    );
    assert!(db.mark_case_merge_group_audited("a").unwrap());
    assert!(db.case_merge_group("a").unwrap().unwrap().audited);
    assert!(!db.mark_case_merge_group_audited("zz").unwrap());
}

#[test]
fn a_block_is_recorded_once_until_it_is_resolved() {
    let (_d, db) = temp_db();
    let b = NewBlock {
        group_id: Some("g".into()),
        scope: "S".into(),
        this: vec!["kingdom/Vol 1.cbz".into()],
        that: vec!["Kingdom/Vol 1.cbz".into()],
        detail: "the archives differ".into(),
    };
    let id = db.record_case_merge_block(&b, 10).unwrap();
    let again = db
        .record_case_merge_block(
            &NewBlock {
                detail: "still differ".into(),
                group_id: None,
                ..b.clone()
            },
            20,
        )
        .unwrap();
    assert_eq!(id, again);
    let got = db.case_merge_block(id).unwrap().unwrap();
    assert_eq!(
        (got.detail.as_str(), got.first_seen, got.group_id.as_deref()),
        ("still differ", 10, Some("g"))
    );
    let other = db
        .record_case_merge_block(
            &NewBlock {
                this: vec!["kingdom/Vol 2.cbz".into()],
                ..b.clone()
            },
            30,
        )
        .unwrap();
    assert_ne!(other, id);
    assert!(db.resolve_case_merge_block(id, "keep-that", 40).unwrap());
    assert!(!db.resolve_case_merge_block(id, "keep-this", 50).unwrap());
    let open: Vec<i64> = db
        .case_merge_blocks(true)
        .unwrap()
        .into_iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(open, [other]);
    assert_eq!(db.case_merge_blocks(false).unwrap().len(), 2);
    let done = db.case_merge_block(id).unwrap().unwrap();
    assert_eq!(
        (done.resolved_at, done.resolution.as_deref()),
        (Some(40), Some("keep-that"))
    );
    // Resolved, the same block seen again is a new one.
    assert_ne!(db.record_case_merge_block(&b, 60).unwrap(), id);
}

#[test]
fn the_first_owner_snapshot_of_a_loser_is_kept() {
    let (_d, db) = temp_db();
    db.record_series_folder_owner("Straße", "alice").unwrap();
    db.record_volume_upload("Straße/Vol 1.cbz", "bob").unwrap();
    db.record_volume_upload("Straße/Vol 2.cbz", "alice")
        .unwrap();
    db.record_volume_upload("Straße/Vol 3.cbz", "bob").unwrap();
    // Another folder's volumes are not the loser's.
    db.record_volume_upload("STRASSE/Vol 4.cbz", "carol")
        .unwrap();
    db.record_volume_upload("Straße 2/Vol 1.cbz", "dave")
        .unwrap();
    assert!(
        db.take_case_merge_owner_snapshot("/Straße/", "STRASSE", 7)
            .unwrap()
    );
    // Rows moved by a step, and a later group, do not change it.
    db.rename_volume_upload("Straße/Vol 1.cbz", "STRASSE/Vol 1.cbz")
        .unwrap();
    assert!(
        !db.take_case_merge_owner_snapshot("Straße", "Other", 8)
            .unwrap()
    );
    let s = db.case_merge_owner_snapshot("Straße").unwrap().unwrap();
    assert_eq!(s.winner, "STRASSE");
    assert_eq!(s.loser_owner.as_deref(), Some("alice"));
    assert_eq!(s.loser_uploaders, ["alice", "bob"]);
    assert_eq!(s.taken_at, 7);
    // A folder with no owner record and no tracked volume.
    assert!(
        db.take_case_merge_owner_snapshot("Empty", "EMPTY", 9)
            .unwrap()
    );
    let e = db.case_merge_owner_snapshot("Empty").unwrap().unwrap();
    assert_eq!((e.loser_owner, e.loser_uploaders.len()), (None, 0));
    assert!(db.forget_case_merge_owner_snapshot("Straße").unwrap());
    assert!(!db.forget_case_merge_owner_snapshot("Straße").unwrap());
    assert!(db.case_merge_owner_snapshot("Straße").unwrap().is_none());
    assert!(!db.take_case_merge_owner_snapshot("/", "x", 1).unwrap());
}

#[test]
fn deferred_adoptions_gather_spellings_by_series() {
    let (_d, db) = temp_db();
    assert_eq!(
        db.defer_series_folder_adoption(&["Kingdom".into(), "kingdom".into()])
            .unwrap(),
        1
    );
    assert_eq!(
        db.defer_series_folder_adoption(&["KINGDOM/".into(), "Dr  Stone".into(), "".into()])
            .unwrap(),
        2
    );
    let all = db.deferred_series_folder_adoptions().unwrap();
    assert_eq!(
        all,
        [
            ("dr stone".to_string(), vec!["Dr  Stone".to_string()]),
            (
                "kingdom".to_string(),
                vec!["KINGDOM".into(), "Kingdom".into(), "kingdom".into()]
            ),
        ]
    );
    assert_eq!(
        db.deferred_series_folder_adoption("kInGdOm").unwrap(),
        Some(vec!["KINGDOM".into(), "Kingdom".into(), "kingdom".into()])
    );
    assert!(
        db.forget_deferred_series_folder_adoption("Kingdom")
            .unwrap()
    );
    assert!(
        !db.forget_deferred_series_folder_adoption("kingdom")
            .unwrap()
    );
    assert_eq!(db.deferred_series_folder_adoption("kingdom").unwrap(), None);
    assert_eq!(db.deferred_series_folder_adoptions().unwrap().len(), 1);
}

#[test]
fn a_row_this_build_cannot_read_is_an_error() {
    let (_d, db) = temp_db();
    db.open_case_merge_group(&group("g", "{}", 1), &[mv("a", "b")])
        .unwrap();
    db.with_writer_connection(|c| {
        c.execute("UPDATE case_merge_steps SET state = 'molten'", [])
            .unwrap()
    });
    assert!(db.case_merge_steps("g").is_err());
    db.with_writer_connection(|c| {
        c.execute("UPDATE case_merge_groups SET state = 'molten'", [])
            .unwrap()
    });
    assert!(db.case_merge_group("g").is_err());
}

#[test]
fn text_forms_round_trip() {
    for k in [
        StepKind::Move,
        StepKind::Keep,
        StepKind::Replace,
        StepKind::Submit,
        StepKind::Finish,
        StepKind::OverlayFold,
        StepKind::Close,
    ] {
        assert_eq!(StepKind::parse(k.as_str()), Some(k));
    }
    for s in [
        StepState::Planned,
        StepState::Copying,
        StepState::Copied,
        StepState::Kept,
        StepState::Done,
        StepState::Skipped,
        StepState::Diverged,
        StepState::Blocked,
    ] {
        assert_eq!(StepState::parse(s.as_str()), Some(s));
        assert_eq!(s.to_string(), s.as_str());
    }
    for g in [GroupState::Open, GroupState::Paused, GroupState::Done] {
        assert_eq!(GroupState::parse(g.as_str()), Some(g));
    }
    assert_eq!(StepKind::parse("trash"), None);
}
