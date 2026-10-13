//! Opt-in check against the real Google Drive API. Read-only: it lists the
//! `mokuro-reader` roots this grant can see and reads one small file.
//!
//! BUNKO_LIVE_GDRIVE_TOKEN=<access token> \
//!     cargo test -p bunko-remote --test gdrive_live -- --ignored --nocapture
#![cfg(feature = "gdrive")]

use std::sync::Arc;

use bunko_remote::backend::{Backend, RNode, read_to_vec};
use bunko_remote::gdrive::{GDriveBackend, GDriveConfig, StaticToken};

#[tokio::test]
#[ignore = "needs BUNKO_LIVE_GDRIVE_TOKEN (a Drive access token)"]
async fn live_lists_roots_and_reads_one_small_file() {
    let Ok(token) = std::env::var("BUNKO_LIVE_GDRIVE_TOKEN") else {
        eprintln!("BUNKO_LIVE_GDRIVE_TOKEN not set; skipping");
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let be = GDriveBackend::new(
        Arc::new(StaticToken::new(token)),
        GDriveConfig::new(state.path()),
    )
    .unwrap();
    let roots = be.locate_roots("mokuro-reader").await.unwrap();
    eprintln!(
        "{} root(s): {:?}",
        roots.len(),
        roots.iter().map(|r| &r.pid).collect::<Vec<_>>()
    );
    let mut small: Option<RNode> = None;
    'outer: for root in &roots {
        let mut dirs = vec![root.pid.clone()];
        while let Some(d) = dirs.pop() {
            let mut kids = Vec::new();
            be.list_dir(&d, &mut |n| kids.push(n)).await.unwrap();
            for k in kids {
                if k.is_dir {
                    dirs.push(k.pid);
                } else if k.size > 0 && k.size <= 1 << 20 {
                    small = Some(k);
                    break 'outer;
                }
            }
        }
    }
    let Some(f) = small else {
        eprintln!("no small file under the roots; listing only");
        return;
    };
    let pinned = be.read(&f.pid, f.pin.as_deref(), None).await.unwrap();
    let bytes = read_to_vec(pinned).await.unwrap();
    assert_eq!(bytes.len() as u64, f.size);
    eprintln!(
        "read {} ({} bytes, pinned to {:?}); {} quota units",
        String::from_utf8_lossy(&f.name_raw),
        bytes.len(),
        f.pin,
        be.units_used()
    );
}
