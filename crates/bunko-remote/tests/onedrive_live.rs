//! Opt-in check against the real Microsoft Graph API. Read-only: it locates the
//! `mokuro-reader` root, lists it, and takes a delta cursor.
//!
//! BUNKO_LIVE_ONEDRIVE_TOKEN=<access token> \
//!     cargo test -p bunko-remote --test onedrive_live -- --ignored --nocapture
#![cfg(feature = "onedrive")]

use std::sync::Arc;

use bunko_remote::backend::Backend;
use bunko_remote::onedrive::{OneDriveBackend, OneDriveConfig, StaticToken};

#[tokio::test]
#[ignore = "needs BUNKO_LIVE_ONEDRIVE_TOKEN (a Graph access token with Files.Read)"]
async fn live_lists_the_root_folder() {
    let Ok(token) = std::env::var("BUNKO_LIVE_ONEDRIVE_TOKEN") else {
        eprintln!("BUNKO_LIVE_ONEDRIVE_TOKEN not set; skipping");
        return;
    };
    let be =
        OneDriveBackend::new(Arc::new(StaticToken::new(token)), OneDriveConfig::default()).unwrap();
    let acct = be.account().await.unwrap();
    eprintln!("account {} ({})", acct.label, acct.id);
    let roots = be.locate_roots("mokuro-reader").await.unwrap();
    let Some(root) = roots.first() else {
        eprintln!("no mokuro-reader folder at the drive root; nothing to list");
        return;
    };
    let mut kids = Vec::new();
    be.list_dir(&root.pid, &mut |n| kids.push(n)).await.unwrap();
    for k in kids.iter().take(20) {
        eprintln!(
            "{} {:>10} {} cver={} hash={:?}",
            if k.is_dir { "d" } else { "-" },
            k.size,
            String::from_utf8_lossy(&k.name_raw),
            k.cver,
            k.hash
        );
    }
    let cursor = be.start_cursor().await.unwrap();
    eprintln!(
        "{} entries; cursor {} chars; {} resource units",
        kids.len(),
        cursor.0.len(),
        be.units_used()
    );
}
