//! `series.json` rules every store shares.

/// A `series.json` without its index data (`volumes`, `spine_offset`): `None` when it
/// has none to drop, or is no JSON object (the server's merge refuses it as it is).
pub fn facts_only(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(bytes);
    let serde_json::Value::Object(mut map) = serde_json::from_slice(text).ok()? else {
        return None;
    };
    let dropped = map.remove("volumes").is_some() | map.remove("spine_offset").is_some();
    dropped.then(|| serde_json::to_vec(&map).expect("a JSON map serialises"))
}

#[cfg(test)]
mod tests {
    use super::facts_only;

    #[test]
    fn facts_only_drops_the_index_data_and_keeps_the_facts() {
        let out = facts_only(br#"{"version":2,"titles":["K"],"volumes":[1],"spine_offset":3}"#)
            .expect("index data to drop");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v, serde_json::json!({"version": 2, "titles": ["K"]}));
        // A byte-order mark is skipped.
        let bom = [b"\xEF\xBB\xBF".as_slice(), br#"{"volumes":[]}"#].concat();
        assert_eq!(facts_only(&bom).as_deref(), Some(b"{}".as_slice()));
    }

    #[test]
    fn facts_only_is_none_when_there_is_nothing_to_drop_or_no_object() {
        assert_eq!(facts_only(br#"{"version":2,"titles":["K"]}"#), None);
        assert_eq!(facts_only(b"[1,2]"), None);
        assert_eq!(facts_only(b"not json"), None);
    }
}
