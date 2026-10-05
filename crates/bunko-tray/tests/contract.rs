//! The tray reads what the instances send: bunko-control's own wire types (stream G1),
//! serialized, parse into the tray's lenient types with nothing lost that the menu uses.

use bunko_control as bc;
use bunko_tray::model::{self, IconState, InstanceView, UpdateView};
use bunko_tray::status::{ControlFile, Status};

fn full_status() -> bc::Status {
    bc::Status {
        role: bc::Role::Processor,
        version: "0.7.0".into(),
        name: "beast".into(),
        state: bc::State::Paused,
        pause: bc::PauseView {
            mode: Some(bc::PauseMode::Now),
            until: Some("2026-10-05T15:00:00Z".into()),
            reason: Some("user".into()),
            since: Some("2026-10-04T15:00:00Z".into()),
        },
        library: bc::LibraryView {
            url: Some("https://lib.example".into()),
            connected: true,
            queue_pending: Some(12),
            error: None,
        },
        current: vec![bc::CurrentVolume {
            volume: "Dr Stone/Dr Stone 01".into(),
            engine: Some("hayai-nova".into()),
            precision: Some("bf16".into()),
            device: Some("gpu:0 RTX 4090".into()),
            pages_done: 37,
            pages_total: 196,
            pages_per_second: Some(4.2),
            eta_seconds: Some(38),
        }],
        stats: bc::Stats {
            today: bc::DayStats {
                volumes: 3,
                pages: 512,
                busy_seconds: 900,
            },
            total: bc::TotalStats {
                volumes: 41,
                pages: 7310,
            },
            rate_pages_per_minute: 252.0,
            gpu_busy_percent: Some(71.0),
            cpu_cores_busy: Some(6.5),
        },
        backend: bc::BackendView {
            pack: Some("torch-cu130-2.13.0".into()),
            devices: vec!["gpu:0 RTX 4090 sm_89".into()],
        },
        problems: vec![bc::Problem {
            severity: bc::Severity::Warn,
            text: "slow disk".into(),
            hint: None,
        }],
        urls: bc::Urls {
            dashboard: "/app/dashboard".into(),
            library: Some("https://lib.example".into()),
            logs_dir: Some("/var/log/x".into()),
        },
        managed: true,
        can_pause: true,
    }
}

#[test]
fn status_round_trips_into_the_tray_types() {
    let wire = serde_json::to_string(&full_status()).unwrap();
    let s: Status = serde_json::from_str(&wire).unwrap();
    assert_eq!(s.role, "processor");
    assert_eq!(s.state, "paused");
    assert_eq!(s.pause.mode.as_deref(), Some("now"));
    assert_eq!(s.pause.until.as_deref(), Some("2026-10-05T15:00:00Z"));
    assert_eq!(s.library.as_ref().unwrap().queue_pending, Some(12.0));
    assert_eq!(s.current[0].pages_total, Some(196.0));
    assert_eq!(
        s.stats.as_ref().unwrap().total.as_ref().unwrap().pages,
        Some(7310.0)
    );
    assert_eq!(s.problems[0].severity, "warn");
    assert_eq!(s.urls.dashboard.as_deref(), Some("/app/dashboard"));
    assert!(s.managed);
    assert!(s.can_pause());

    let m = model::build(&model::Inputs {
        instances: &[InstanceView {
            role: s.role.clone(),
            status: Some(s),
            error: None,
            tray_started: true,
        }],
        supervised: &[],
        update: &UpdateView::default(),
        notice: None,
        now: chrono::Local::now(),
    });
    assert_eq!(m.icon, IconState::Paused);
    assert!(m.status_lines[0].starts_with("Processor: Paused until "));
    assert!(m.can_resume && !m.can_pause_now);
}

#[test]
fn a_lite_server_cannot_be_paused() {
    let mut st = full_status();
    st.role = bc::Role::Server;
    st.state = bc::State::Idle;
    st.can_pause = false;
    let s: Status = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
    assert!(!s.can_pause());
}

#[test]
fn control_file_round_trips() {
    let f = bc::ControlFile {
        role: bc::Role::Gui,
        pid: 42,
        port: 51234,
        token: "ab".repeat(32),
        version: "0.7.0".into(),
        started_at: "2026-10-04T10:00:00Z".into(),
        url: "http://127.0.0.1:51234".into(),
        managed: false,
    };
    let c: ControlFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
    assert_eq!(c.role, "gui");
    assert_eq!(c.base_url(), "http://127.0.0.1:51234");
    assert_eq!(c.token, f.token);
}
