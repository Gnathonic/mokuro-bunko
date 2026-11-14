//! Checks over the metadata of a set of graphs (`package::GraphSet`). Always built (no
//! libtorch), so the rules are tested in the normal workspace build; the libtorch side
//! reads each graph's metadata and hands the values here.

/// Every graph's `bunko.io` must be `expect` (a graph without one is refused).
pub fn check_io(found: &[Option<&str>], expect: &str) -> Result<(), String> {
    if !found.is_empty() && found.iter().all(|v| v.map(str::trim) == Some(expect)) {
        Ok(())
    } else {
        Err(format!(
            "package I/O {found:?}: this runtime drives {expect} and every graph must say so"
        ))
    }
}

/// The one value every graph gives for `key` (`None` counts as a value: all absent is
/// `Ok(None)`, some absent is a disagreement).
pub fn agreed<'a>(key: &str, found: &[Option<&'a str>]) -> Result<Option<&'a str>, String> {
    let first = found.first().copied().flatten();
    if found.iter().all(|v| *v == first) {
        Ok(first)
    } else {
        Err(format!("the graphs disagree on {key}: {found:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_graph_must_say_the_io() {
        assert!(check_io(&[Some("textbox-1")], "textbox-1").is_ok());
        assert!(check_io(&[Some("textbox-1"), Some("textbox-1")], "textbox-1").is_ok());
        let e = check_io(&[Some("textbox-1"), None], "textbox-1").unwrap_err();
        assert!(e.contains("textbox-1"), "{e}");
        assert!(check_io(&[Some("2")], "textbox-1").is_err());
    }

    #[test]
    fn graphs_must_agree() {
        assert_eq!(agreed("bunko.weights_dtype", &[None, None]).unwrap(), None);
        assert_eq!(
            agreed("k", &[Some("fp16"), Some("fp16")]).unwrap(),
            Some("fp16")
        );
        let e = agreed("bunko.weights_dtype", &[Some("fp16"), None]).unwrap_err();
        assert!(e.contains("bunko.weights_dtype"), "{e}");
    }
}
