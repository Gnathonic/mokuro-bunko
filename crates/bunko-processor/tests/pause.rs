//! The processor's own pause (GUI.md §3) on the session runtime, over the local link
//! with the fake engine: what it says to the library and what it stops doing.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bunko_control::{Control, ControlConfig, PauseMode, Role, State};
use bunko_processor::{
    BenchConfig, FakeConfig, FakePipeline, LocalConfig, LocalLink, LocalProcessor,
};
use bunko_proto::{Availability, Event, Op, VolumeOp};
use common::*;

fn control(storage: &Path) -> Control {
    let c = Control::new(ControlConfig::new(
        Role::Processor,
        "box",
        "0.7.0-test",
        storage,
    ));
    c.set_link(
        bunko_control::LinkPhase::Connected,
        Some("http://library"),
        None,
    );
    c
}

/// The next event that is not page progress.
async fn next_said(link: &mut LocalLink) -> Event {
    loop {
        let e = next_event(&mut link.events, 5).await;
        if !matches!(e, Event::Page { .. } | Event::Stats { .. }) {
            return e;
        }
    }
}

fn start(config: FakeConfig, results: &Path, control: &Control) -> LocalLink {
    LocalProcessor::spawn_controlled(
        Arc::new(FakePipeline::new(config)),
        LocalConfig {
            results_dir: results.to_path_buf(),
        },
        BenchConfig::default(),
        Some(control.clone()),
    )
}

fn volume_op(sid: &str, claim: &str, archive: &Path) -> Op {
    let stem = archive.file_stem().unwrap().to_string_lossy().into_owned();
    Op::Volume(VolumeOp {
        sid: sid.into(),
        claim: claim.into(),
        archive: archive.to_string_lossy().into_owned(),
        sidecar_name: format!("{stem}.mokuro"),
        title: "Series".into(),
        volume_title: stem,
        title_uuid: None,
        volume_uuid: None,
        size: None,
        etag: None,
    })
}

fn open(sid: &str) -> Op {
    Op::OpenSession {
        sid: sid.into(),
        generation: row("fake"),
    }
}

async fn wait_for(link: &mut LocalLink, what: &str, secs: u64) -> Vec<Event> {
    let mut out = Vec::new();
    loop {
        let e = next_event(&mut link.events, secs).await;
        let hit = names(std::slice::from_ref(&e)).first().map(String::as_str) == Some(what);
        out.push(e);
        if hit {
            return out;
        }
    }
}

fn slow() -> FakeConfig {
    FakeConfig {
        page_delay: Duration::from_millis(150),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_after_volume_releases_the_unstarted_and_finishes_the_running() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = control(dir.path());
    let a = write_volume(dir.path(), "Vol 1.cbz", 6);
    let b = write_volume(dir.path(), "Vol 2.cbz", 6);
    let mut link = start(slow(), &dir.path().join("results"), &ctl);
    link.ops.send(open("s1")).await.unwrap();
    link.ops.send(volume_op("s1", "v1", &a)).await.unwrap();
    link.ops.send(volume_op("s1", "v2", &b)).await.unwrap();
    wait_for(&mut link, "volume_started:v1", 10).await;
    assert_eq!(ctl.status().state, State::Working);
    assert_eq!(ctl.status().current[0].volume, "Series/Vol 1");
    ctl.pause_mode(PauseMode::AfterVolume, None).unwrap();
    let said = next_said(&mut link).await;
    assert!(
        matches!(
            &said,
            Event::Availability(Availability { paused: true, .. })
        ),
        "{said:?}"
    );
    let first = next_said(&mut link).await;
    assert_eq!(
        first,
        Event::Released {
            claims: vec!["v2".into()]
        }
    );
    assert_eq!(ctl.status().state, State::Pausing);
    // The running volume finishes and is reported; the released one never runs.
    let rest = wait_for(&mut link, "volume_done:v1", 10).await;
    assert!(
        !names(&rest).iter().any(|n| n.ends_with(":v2")),
        "{rest:#?}"
    );
    assert_eq!(ctl.status().state, State::Paused);
    // The library closes the drained session.
    link.ops
        .send(Op::CloseSession { sid: "s1".into() })
        .await
        .unwrap();
    let tail = until_exit(&mut link.events, "s1", 10).await;
    assert_eq!(names(&tail), ["exit"], "{tail:#?}");
    let (today, _) = ctl.activity().unwrap().counts();
    assert_eq!((today.volumes, today.pages), (1, 6));
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_now_releases_everything_and_says_only_exit() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = control(dir.path());
    let a = write_volume(dir.path(), "Vol 1.cbz", 20);
    let b = write_volume(dir.path(), "Vol 2.cbz", 20);
    let mut link = start(slow(), &dir.path().join("results"), &ctl);
    link.ops.send(open("s1")).await.unwrap();
    link.ops.send(volume_op("s1", "v1", &a)).await.unwrap();
    link.ops.send(volume_op("s1", "v2", &b)).await.unwrap();
    wait_for(&mut link, "volume_started:v1", 10).await;
    let paused_at = std::time::Instant::now();
    ctl.pause_mode(PauseMode::Now, Some("2099-01-01T00:00:00Z"))
        .unwrap();
    let events = until_exit(&mut link.events, "s1", 10).await;
    let told = names(&events);
    assert_eq!(told, ["availability", "released", "exit"], "{events:#?}");
    assert!(paused_at.elapsed() < Duration::from_secs(5));
    assert!(events.contains(&Event::Released {
        claims: vec!["v1".into(), "v2".into()]
    }));
    assert!(events.contains(&Event::Availability(Availability {
        paused: true,
        until: Some("2099-01-01T00:00:00Z".into()),
        reason: Some("user".into()),
    })));
    let status = ctl.status();
    assert_eq!(status.state, State::Paused);
    assert!(status.current.is_empty());
    assert_eq!(status.pause.until.as_deref(), Some("2099-01-01T00:00:00Z"));
    // While paused: work that crosses the pause goes straight back.
    link.ops.send(open("s2")).await.unwrap();
    let e = next_event(&mut link.events, 5).await;
    assert_eq!(
        e,
        Event::Exit {
            sid: "s2".into(),
            returncode: None
        }
    );
    link.ops.send(volume_op("s2", "v3", &a)).await.unwrap();
    assert_eq!(
        next_event(&mut link.events, 5).await,
        Event::Released {
            claims: vec!["v3".into()]
        }
    );
    // Resume: said once, and work is taken again.
    ctl.resume().unwrap();
    assert_eq!(
        next_event(&mut link.events, 5).await,
        Event::Availability(Availability::default())
    );
    link.ops.send(open("s3")).await.unwrap();
    link.ops.send(volume_op("s3", "v4", &b)).await.unwrap();
    wait_for(&mut link, "volume_started:v4", 10).await;
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_survives_a_restart_and_is_announced_first() {
    let dir = tempfile::tempdir().unwrap();
    {
        let ctl = control(dir.path());
        ctl.pause_mode(PauseMode::AfterVolume, None).unwrap();
    }
    // The next run of the processor starts paused and says so before anything else.
    let ctl = control(dir.path());
    assert_eq!(ctl.status().state, State::Paused);
    let mut link = start(FakeConfig::default(), &dir.path().join("results"), &ctl);
    let first = next_event(&mut link.events, 5).await;
    assert_eq!(
        first,
        Event::Availability(Availability {
            paused: true,
            until: None,
            reason: Some("user".into())
        })
    );
    link.ops.send(open("s1")).await.unwrap();
    assert!(matches!(
        next_event(&mut link.events, 5).await,
        Event::Exit { .. }
    ));
    link.shutdown().await;
}
