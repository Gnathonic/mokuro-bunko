//! Opt-in check against the real MEGA API. Read-only: it resumes a stored session,
//! locates the `mokuro-reader` roots and lists them.
//!
//! BUNKO_LIVE_MEGA_SESSION=<serialised session blob> \
//!     cargo test -p bunko-remote --test mega_live -- --ignored --nocapture
//!
//! Get a blob once with `MegaSession::login` (email, password, 2FA code); the blob
//! is password-equivalent: do not paste it anywhere it is kept.
#![cfg(feature = "mega")]

use bunko_remote::backend::Backend;
use bunko_remote::mega::{MegaBackend, MegaConfig, SessionBlob};

#[tokio::test]
#[ignore = "needs BUNKO_LIVE_MEGA_SESSION (a serialised MEGA session)"]
async fn live_lists_the_roots() {
    let Ok(blob) = std::env::var("BUNKO_LIVE_MEGA_SESSION") else {
        eprintln!("BUNKO_LIVE_MEGA_SESSION not set; skipping");
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let be = MegaBackend::resume(&SessionBlob::new(blob), MegaConfig::new(state.path()))
        .await
        .unwrap();
    let roots = be.locate_roots("mokuro-reader").await.unwrap();
    eprintln!(
        "{} root(s): {:?}",
        roots.len(),
        roots.iter().map(|r| &r.pid).collect::<Vec<_>>()
    );
    let pids: Vec<String> = roots.iter().map(|r| r.pid.clone()).collect();
    let mut n = 0usize;
    let mut bytes = 0u64;
    be.list_all(&pids, &mut |node| {
        n += 1;
        bytes += node.size;
    })
    .await
    .unwrap();
    let c = be.start_cursor().await.unwrap();
    eprintln!("{n} nodes, {bytes} bytes under the roots; sn {}", c.0);
}
