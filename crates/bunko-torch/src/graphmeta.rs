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

/// The dtype a package set's weights are cast to at load (`bunko.weights_dtype`, which
/// every graph must give alike): `None` binds the weights as stored, `Some("fp16")` casts
/// their floating tensors to fp16 (a package compiled at fp16 over the authors' fp32
/// file). Any other value is refused.
pub fn weights_cast(found: &[Option<&str>]) -> Result<Option<&'static str>, String> {
    match agreed("bunko.weights_dtype", found)? {
        None => Ok(None),
        Some("fp16") => Ok(Some("fp16")),
        Some(v) => Err(format!(
            "bunko.weights_dtype {v}: this runtime casts to fp16 only"
        )),
    }
}

/// Whether a nar-small package set loads at `precision` given its agreed
/// `bunko.weights_dtype` ([`weights_cast`]): fp32 binds the authors' weights as stored
/// (no cast), fp16 is the authors' fp16 mode, a package that casts them to fp16. bf16
/// has no package. A package whose cast is not the precision asked for (a hand-made or
/// mixed package directory) is refused, so a session never runs at another precision
/// than it reports.
pub fn nar_precision(precision: &str, weights_dtype: Option<&str>) -> Result<(), String> {
    match (precision, weights_dtype) {
        ("fp32", None) | ("fp16", Some("fp16")) => Ok(()),
        ("fp32" | "fp16", found) => Err(format!(
            "nar-small {precision}: the package's bunko.weights_dtype is {}",
            found.unwrap_or("absent")
        )),
        (other, _) => Err(format!("nar-small runs at fp32 or fp16, not {other}")),
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

    #[test]
    fn weights_dtype_must_agree() {
        assert_eq!(weights_cast(&[None, None]).unwrap(), None);
        assert_eq!(weights_cast(&[Some("fp16")]).unwrap(), Some("fp16"));
        assert!(
            weights_cast(&[Some("fp16"), None])
                .unwrap_err()
                .contains("bunko.weights_dtype")
        );
        assert!(
            weights_cast(&[Some("bf16")])
                .unwrap_err()
                .contains("fp16 only")
        );
    }

    /// nar-small loads at fp32 (weights as stored) or fp16 (a package that casts them),
    /// never bf16, and never a package whose cast says otherwise than the precision
    /// asked for (a hand-made or mixed package directory).
    #[test]
    fn nar_loads_the_precision_its_package_is() {
        assert!(nar_precision("fp32", None).is_ok());
        assert!(nar_precision("fp16", Some("fp16")).is_ok());
        let e = nar_precision("bf16", None).unwrap_err();
        assert!(e.contains("fp32 or fp16, not bf16"), "{e}");
        let e = nar_precision("fp16", None).unwrap_err();
        assert!(e.contains("bunko.weights_dtype"), "{e}");
        let e = nar_precision("fp32", Some("fp16")).unwrap_err();
        assert!(e.contains("bunko.weights_dtype"), "{e}");
    }
}
