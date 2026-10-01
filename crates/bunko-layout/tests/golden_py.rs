//! Golden: the CPython semantics everything else rests on — `math.hypot`,
//! `round(x, n)`, 3.12's compensated `sum`, `repr(float)`, libm trig/pow,
//! `str.isspace`, and NFKC over every code point.

mod common;

use bunko_layout::json::Value;
use bunko_layout::py;
use common::*;
use unicode_normalization::UnicodeNormalization;

#[test]
fn float_semantics_match_cpython() {
    let v = load("cases/pyfloat.json.gz");
    let mut errs = Vec::new();
    for c in arr(get(&v, "hypot")) {
        let c = arr(c);
        let got = py::hypot(f(&c[0]), f(&c[1]));
        if !same(got, f(&c[2])) {
            errs.push(format!("hypot({}, {}) = {got:e} != {:e}", f(&c[0]), f(&c[1]), f(&c[2])));
        }
    }
    for c in arr(get(&v, "round")) {
        let c = arr(c);
        let got = py::round_digits(f(&c[0]), i(&c[1]) as u32);
        if !same(got, f(&c[2])) {
            errs.push(format!("round({:e}, {}) = {got:e} != {:e}", f(&c[0]), i(&c[1]), f(&c[2])));
        }
    }
    for c in arr(get(&v, "sum")) {
        let c = arr(c);
        let vals: Vec<f64> = arr(&c[0]).iter().map(f).collect();
        let got = py::sum(vals.iter().copied());
        if !same(got, f(&c[1])) {
            errs.push(format!("sum({vals:?}) = {got:e} != {:e}", f(&c[1])));
        }
    }
    for c in arr(get(&v, "repr")) {
        let c = arr(c);
        let got = py::float_repr(f(&c[0]));
        if got != s(&c[1]) {
            errs.push(format!("repr: {got} != {}", s(&c[1])));
        }
    }
    for c in arr(get(&v, "trig")) {
        let c: Vec<f64> = arr(c).iter().map(f).collect();
        let (x, y) = (c[0], c[1]);
        let got = [
            py::degrees(y.atan2(x)),
            py::radians(x).cos(),
            py::radians(x).sin(),
            py::pow(x * x + y * y, 0.5),
            py::pow(x, 2.0),
        ];
        for (k, g) in got.iter().enumerate() {
            if !same(*g, c[k + 2]) {
                errs.push(format!("trig[{k}]({x:e}, {y:e}) = {g:e} != {:e}", c[k + 2]));
            }
        }
    }
    assert!(errs.is_empty(), "{} differ:\n{}", errs.len(), errs.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn isspace_matches_python() {
    let v = load("cases/pyfloat.json.gz");
    let want: std::collections::BTreeSet<u32> = arr(get(&v, "isspace")).iter().map(|x| i(x) as u32).collect();
    let got: std::collections::BTreeSet<u32> =
        (0..=0x10FFFFu32).filter_map(char::from_u32).filter(|c| py::is_space(*c)).map(|c| c as u32).collect();
    assert_eq!(got, want);
}

/// NFKC per character against Python 3.12 (Unicode 15.0). Every character
/// assigned in 15.0 must agree; characters Python considers unassigned may
/// differ (newer Unicode in the crate) and are only reported.
#[test]
fn nfkc_matches_python_on_assigned_characters() {
    let v = load("cases/pyfloat.json.gz");
    let Value::Object(map) = get(&v, "nfkc") else { panic!("nfkc map") };
    let want: std::collections::HashMap<u32, &str> =
        map.iter().map(|(k, v)| (k.parse::<u32>().expect("cp"), s(v))).collect();
    let unassigned: Vec<(u32, u32)> =
        arr(get(&v, "unassigned")).iter().map(|r| (i(&arr(r)[0]) as u32, i(&arr(r)[1]) as u32)).collect();
    let is_unassigned = |cp: u32| unassigned.iter().any(|&(a, b)| a <= cp && cp <= b);
    let mut errs = Vec::new();
    let mut drift = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(ch) = char::from_u32(cp) else { continue };
        let got: String = std::iter::once(ch).nfkc().collect();
        let expect = want.get(&cp).map(|s| s.to_string()).unwrap_or_else(|| ch.to_string());
        if got != expect {
            if is_unassigned(cp) {
                drift.push(cp);
            } else {
                errs.push(format!("U+{cp:04X}: {got:?} != {expect:?}"));
            }
        }
    }
    eprintln!(
        "NFKC: Python unidata {}; {} code points unassigned there fold differently here (newer Unicode)",
        s(get(&v, "unidata_version")),
        drift.len()
    );
    assert!(errs.is_empty(), "{} differ:\n{}", errs.len(), errs.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}
