//! Provenance through an older reader's rewrite (critique C5).
//!
//! Mokuro Reader 1.10.0 regenerates a volume's main sidecar from its own copy of the
//! pages when it renames the volume (new title, new file name), and writes it without
//! the `ocr_engine` block: that reader keeps nothing else. When the pages it sends are
//! exactly a sealed main's (the same RFC 8785 text, [`bunko_layout::canon`]), they ARE
//! the model output that block names, so the PUT gets the block back and is sealed
//! again before it is published. The sealed main is the one at the PUT's own path (a
//! re-upload, a title change), else the main of the same `volume_uuid` in the folder
//! (the rename uploads the new main before it moves the archive and deletes the old
//! one). A page that differs, a layer, a `.gz`, a file a reader marked EDITED or one
//! with a block of its own is stored as sent.
//!
//! Accepted limits: a 1.10.0 reader's page edit still lands in the main (diverting it
//! would break that reader's sync); D4's `pre-upgrade` keeps it before any upgrade
//! replaces it. A rename into another series folder finds no sealed main beside it.

use std::path::Path;

use bunko_core::sidecar_names::{self, SidecarClass};
use bunko_layout::json::Value;
use bunko_library::sidecar;
use bunko_store::fs;

/// Put Bunko's block back on the staged body of a main sidecar PUT to `dest`, when its
/// pages are a sealed main's (see the module). Returns whether `staged` was rewritten.
pub fn reattach_on_put(dest: &Path, staged: &Path) -> bool {
    let Some(text) = reattached(dest, staged) else {
        return false;
    };
    let written = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(staged)?;
        f.write_all(text.as_bytes())?;
        f.flush()
    })();
    match written {
        Ok(()) => {
            tracing::info!(
                "Kept the OCR provenance of {} through a reader's rewrite (same pages)",
                dest.display()
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                "could not keep the OCR provenance of {}: {e}",
                dest.display()
            );
            false
        }
    }
}

fn parse(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    Value::parse(text.strip_prefix('\u{feff}').unwrap_or(text)).ok()
}

/// The staged body with the sealed main's block on it, sealed, compact.
fn reattached(dest: &Path, staged: &Path) -> Option<String> {
    let name = dest.file_name()?.to_str()?;
    if sidecar_names::split_mokuro(name)?.1 {
        return None;
    }
    let dir = dest.parent()?;
    let listed = sidecar::list_names(dir).unwrap_or_default();
    let names: Vec<&str> = listed.iter().map(String::as_str).collect();
    let stems = sidecar_names::volume_stems(names.iter().copied());
    if matches!(
        sidecar_names::classify(name, &stems),
        SidecarClass::Layer { .. }
    ) {
        return None;
    }
    let max = sidecar::MAX_SIDECAR_BYTES;
    let mut doc = parse(&sidecar::read_capped(staged, max).ok()?)?;
    if !doc.is_object() || doc.get("ocr_engine").is_some() || doc.get("edited").is_some() {
        return None;
    }
    let pages = bunko_layout::canon::jcs(doc.get("pages").filter(|p| p.as_array().is_some())?)?;
    let uuid = doc
        .get("volume_uuid")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let block = sealed_block(dest, &pages, None).or_else(|| {
        let uuid = uuid.as_deref()?;
        names
            .iter()
            .filter(|n| **n != name)
            .filter(|n| {
                matches!(
                    sidecar_names::classify(n, &stems),
                    SidecarClass::Primary { gz: false, .. }
                )
            })
            .find_map(|n| sealed_block(&dir.join(n), &pages, Some(uuid)))
    })?;
    doc.set("ocr_engine", block);
    bunko_layout::seal::seal(&mut doc)
}

/// The `ocr_engine` block of the sealed sidecar at `path` whose pages' canonical text is
/// `pages` (and whose `volume_uuid` is `uuid`, when given).
fn sealed_block(path: &Path, pages: &str, uuid: Option<&str>) -> Option<Value> {
    let bytes = sidecar::read_capped(path, sidecar::MAX_SIDECAR_BYTES).ok()?;
    // Most mains in a folder are other volumes: skip them before parsing.
    if let Some(uuid) = uuid
        && !bytes.windows(uuid.len()).any(|w| w == uuid.as_bytes())
    {
        return None;
    }
    if !bunko_layout::seal::is_sealed(&bytes) {
        return None;
    }
    let data = parse(&bytes)?;
    if uuid.is_some() && data.get("volume_uuid").and_then(Value::as_str) != uuid {
        return None;
    }
    let same = bunko_layout::canon::jcs(data.get("pages")?).is_some_and(|p| p == pages);
    same.then(|| data.get("ocr_engine").cloned()).flatten()
}
