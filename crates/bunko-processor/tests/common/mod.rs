#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bunko_proto::{Event, RowSpec};
use tokio::sync::mpsc;

/// A stored (uncompressed) zip of `pages`, so bytes can be damaged at known places.
pub fn stored_zip(pages: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in pages {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }
    out.into_inner()
}

/// A volume of `n` fake page images, each `size` bytes of a recognisable pattern.
pub fn volume(n: usize, size: usize) -> Vec<u8> {
    let pages: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| {
            let data: Vec<u8> = (0..size).map(|j| ((i * 31 + j * 7) % 251) as u8).collect();
            (format!("{:03}.jpg", i + 1), data)
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = pages
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    stored_zip(&refs)
}

pub fn write_volume(dir: &Path, name: &str, pages: usize) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, volume(pages, 64)).unwrap();
    path
}

pub fn row(engine: &str) -> RowSpec {
    serde_json::from_value(serde_json::json!({"id": "g1", "name": "default", "engine": engine}))
        .unwrap()
}

/// The next event, or panic after `secs`.
pub async fn next_event(events: &mut mpsc::UnboundedReceiver<Event>, secs: u64) -> Event {
    match tokio::time::timeout(Duration::from_secs(secs), events.recv()).await {
        Ok(Some(e)) => e,
        Ok(None) => panic!("the event channel closed"),
        Err(_) => panic!("no event within {secs}s"),
    }
}

/// Every event up to and including the session's `exit`.
pub async fn until_exit(
    events: &mut mpsc::UnboundedReceiver<Event>,
    sid: &str,
    secs: u64,
) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(left, events.recv()).await {
            Ok(Some(e)) => e,
            Ok(None) => panic!("the event channel closed; so far: {out:#?}"),
            Err(_) => panic!("no exit within {secs}s; so far: {out:#?}"),
        };
        let done = matches!(&event, Event::Exit { sid: s, .. } if s == sid);
        out.push(event);
        if done {
            return out;
        }
    }
}

/// Nothing more arrives for `millis`.
pub async fn quiet_for(events: &mut mpsc::UnboundedReceiver<Event>, millis: u64) {
    if let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(millis), events.recv()).await {
        panic!("unexpected event {e:?}");
    }
}

/// Short names of events, for order assertions: `volume_done:v1`, `exit`, ...
pub fn names(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| !matches!(e, Event::Stats { .. } | Event::Page { .. } | Event::Ping))
        .map(|e| {
            let v = serde_json::to_value(e).unwrap();
            let name = v["event"].as_str().unwrap().to_string();
            match e.claim() {
                Some(c) => {
                    if let Event::Fetch { state, .. } = e {
                        format!("{name}:{state}:{c}")
                    } else {
                        format!("{name}:{c}")
                    }
                }
                None => name,
            }
        })
        .collect()
}
